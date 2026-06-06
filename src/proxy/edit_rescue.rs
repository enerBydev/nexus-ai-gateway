//! Edit Rescue Middleware — fuzzy Unicode matching for Edit tool failures
//!
//! When Claude Code's Edit tool fails with "String to replace not found",
//! this module attempts a fuzzy match by normalizing Unicode characters
//! bidirectionally (Unicode -> ASCII and ASCII -> Unicode).
//!
//! Only rescues when:
//! - Error is exactly "String to replace not found"
//! - The fuzzy match is unique (or replace_all=true)
//! - Zero risk of data loss — original error returned on any ambiguity

use std::path::Path;
use tokio::fs;

// Note: Since the Unicode sanitizer already ran (P1), source files should
// only have ASCII. This module helps when:
// 1. CC normalizes \uXXXX escapes in old_string (CC bug #52813)
// 2. CC drifts full-width to half-width (CC bug #52482)
// 3. The file being edited still contains Unicode (external files)

/// Bidirectional Unicode <-> ASCII mapping for fuzzy Edit matching.
/// Each entry maps a Unicode character to its ASCII equivalent.
#[allow(dead_code)] // TODO: Wire into streaming.rs pipeline (Issue #88)
const UNICODE_ASCII_MAP: &[(&str, &str)] = &[
    ("\u{2192}", "->"),                // RIGHTWARDS ARROW
    ("\u{23F1}\u{FE0F}", "[TIMEOUT]"), // STOPWATCH + VS16
    ("\u{26A0}\u{FE0F}", "[WARN]"),    // WARNING SIGN + VS16
    ("\u{2713}", "[OK]"),              // CHECK MARK
    ("\u{2717}", "[FAIL]"),            // BALLOT X
    ("\u{1F50D}", "[SCAN]"),           // LEFT-POINTING MAGNIFYING GLASS
    ("\u{1F4D0}", "[CALIB]"),          // TRIANGULAR RULER
    ("\u{1F6E1}\u{FE0F}", "[GUARD]"),  // SHIELD + VS16
    ("\u{1F4CB}", "[TODO]"),           // CLIPBOARD
    ("\u{1F680}", "[LAUNCH]"),         // ROCKET
    ("\u{1F4CD}", "[PIN]"),            // ROUND PUSHPIN
];

/// Parameters extracted from an Edit tool call
#[allow(dead_code)] // TODO: Wire into streaming.rs pipeline (Issue #88)
#[derive(Debug)]
pub struct EditParams {
    pub file_path: String,
    pub old_string: String,
    pub new_string: String,
    pub replace_all: bool,
}

/// Result of an Edit rescue attempt
#[allow(dead_code)] // TODO: Wire into streaming.rs pipeline (Issue #88)
#[derive(Debug, PartialEq)]
pub enum EditRescueResult {
    /// Fuzzy match succeeded — file was written with the new content
    Rescued { new_content: String },
    /// Could not rescue — original error should be returned
    NotRescuable { reason: String },
}

/// Attempts to rescue a failed Edit by applying fuzzy Unicode matching.
/// Returns None if the error is not a "String to replace not found" error.
/// Returns Some(Rescued) if the fuzzy match succeeded.
/// Returns Some(NotRescuable) if no variant matched.
///
/// This is ASYNC — reads/writes files via tokio::fs without blocking.
#[allow(dead_code)] // TODO: Wire into streaming.rs pipeline (Issue #88)
pub async fn maybe_rescue_edit(
    edit_params: &EditParams,
    error_msg: &str,
) -> Option<EditRescueResult> {
    if !error_msg.contains("String to replace not found") {
        return None; // Not a matching error
    }

    // 1. Read file from disk (async, non-blocking)
    let file_path = Path::new(&edit_params.file_path);

    // Security: Only allow edits within the project directory
    // (prevent path traversal)
    // Check the original path before canonicalization to prevent symlink attacks
    if edit_params.file_path.contains("..") {
        tracing::warn!(
            "Edit rescue rejected: path contains '..' traversal: {}",
            edit_params.file_path
        );
        return None;
    }

    // Security: Restrict rescue to src/ directory only (conservative initial rollout).
    // TODO: Consider expanding to tests/, examples/, etc. after validation (Issue #88).
    let file_path_str = file_path.to_str().unwrap_or("");
    if !file_path_str.starts_with("src/") && !file_path_str.starts_with("./src/") {
        tracing::warn!(
            "Edit rescue rejected: path outside src directory: {}",
            edit_params.file_path
        );
        return None;
    }

    let canonical_path = match fs::canonicalize(file_path).await {
        Ok(path) => path,
        Err(e) => {
            tracing::debug!(
                "Edit rescue: failed to canonicalize path {}: {}",
                edit_params.file_path,
                e
            );
            return None;
        }
    };

    let cwd = std::env::current_dir().ok()?;
    if !canonical_path.starts_with(&cwd) {
        tracing::warn!(
            "Edit rescue rejected: canonicalized path outside project directory: {} -> {:?}",
            edit_params.file_path,
            canonical_path
        );
        return None;
    }

    let file_content = fs::read_to_string(&canonical_path).await.ok()?;

    // 2. Generate fuzzy variants of old_string
    let variants = generate_fuzzy_variants(&edit_params.old_string);

    // 3. Try match with each variant
    for (variant_idx, variant) in variants.iter().enumerate() {
        if let Some(replacement) =
            try_replace(&file_content, variant, &edit_params.new_string, edit_params.replace_all)
        {
            // 4. Verify uniqueness (prevent false positives)
            let match_count = file_content.matches(variant).count();
            if match_count == 1 || edit_params.replace_all {
                // 5. Write modified file (async, non-blocking)
                if fs::write(&canonical_path, &replacement).await.is_ok() {
                    tracing::info!(
                        "Edit rescue: fuzzy match applied in {} (variant #{}, {} chars)",
                        edit_params.file_path,
                        variant_idx,
                        variant.len()
                    );
                    return Some(EditRescueResult::Rescued { new_content: replacement });
                }
            } else {
                tracing::warn!(
                    "Edit rescue rejected: {} ambiguous matches for variant #{} in {}",
                    match_count,
                    variant_idx,
                    edit_params.file_path
                );
            }
        }
    }

    Some(EditRescueResult::NotRescuable {
        reason: "No Unicode variant produced a unique match".to_string(),
    })
}

/// Generates multiple normalized variants of the search string.
#[allow(dead_code)] // TODO: Wire into streaming.rs pipeline (Issue #88)
fn generate_fuzzy_variants(original: &str) -> Vec<String> {
    let mut variants = vec![original.to_string()];

    // Strategy 1: Replace Unicode with ASCII
    let mut ascii_version = original.to_string();
    for (unicode, ascii) in UNICODE_ASCII_MAP {
        ascii_version = ascii_version.replace(unicode, ascii);
    }
    if ascii_version != original {
        variants.push(ascii_version);
    }

    // Strategy 2: Replace ASCII with Unicode (bidirectional)
    let mut unicode_version = original.to_string();
    for (unicode, ascii) in UNICODE_ASCII_MAP {
        unicode_version = unicode_version.replace(ascii, unicode);
    }
    if unicode_version != original {
        variants.push(unicode_version);
    }

    variants
}

/// Attempts string replacement, returns new content if old string is found.
#[allow(dead_code)] // TODO: Wire into streaming.rs pipeline (Issue #88)
fn try_replace(content: &str, old: &str, new: &str, replace_all: bool) -> Option<String> {
    if !content.contains(old) {
        return None;
    }
    if replace_all {
        Some(content.replace(old, new))
    } else {
        let idx = content.find(old)?;
        let mut result = content.to_string();
        result.replace_range(idx..idx + old.len(), new);
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_edit_rescue_not_matching_error() {
        let params = EditParams {
            file_path: "/tmp/test.rs".to_string(),
            old_string: "foo".to_string(),
            new_string: "bar".to_string(),
            replace_all: false,
        };
        let result = maybe_rescue_edit(&params, "other error").await;
        assert_eq!(result, None);
    }

    #[test]
    fn test_generate_fuzzy_variants_arrow_unicode() {
        // Input has Unicode arrow -> Strategy 1 (Unicode->ASCII) produces a variant
        // Strategy 2 (ASCII->Unicode) finds no "->" to replace, so only 2 variants total
        let variants = generate_fuzzy_variants("\u{2192} result");
        assert_eq!(variants.len(), 2); // original + ascii version
        assert!(variants[1].contains("->"));
    }

    #[test]
    fn test_generate_fuzzy_variants_bidirectional() {
        // Input with ASCII "->" -> Strategy 2 produces a Unicode variant
        let variants = generate_fuzzy_variants("foo -> bar");
        assert!(variants.len() >= 2); // at least original + unicode version
        assert!(variants.iter().any(|v| v.contains('\u{2192}')));
    }

    #[test]
    fn test_generate_fuzzy_variants_ascii_only() {
        let variants = generate_fuzzy_variants("plain ascii");
        assert_eq!(variants.len(), 1); // only original
    }

    #[test]
    fn test_try_replace_found() {
        let content = "hello world";
        let result = try_replace(content, "world", "rust", false);
        assert_eq!(result, Some("hello rust".to_string()));
    }

    #[test]
    fn test_try_replace_not_found() {
        let content = "hello world";
        let result = try_replace(content, "missing", "rust", false);
        assert_eq!(result, None);
    }

    #[test]
    fn test_try_replace_all() {
        let content = "aaa bbb aaa";
        let result = try_replace(content, "aaa", "ccc", true);
        assert_eq!(result, Some("ccc bbb ccc".to_string()));
    }
}
