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

use crate::models::anthropic::{self, ContentBlock, MessageContent};
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::fs;

// Note: Since the Unicode sanitizer already ran (P1), source files should
// only have ASCII. This module helps when:
// 1. CC normalizes \uXXXX escapes in old_string (CC bug #52813)
// 2. CC drifts full-width to half-width (CC bug #52482)
// 3. The file being edited still contains Unicode (external files)

/// Bidirectional Unicode <-> ASCII mapping for fuzzy Edit matching.
/// Each entry maps a Unicode character to its ASCII equivalent.
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
#[derive(Debug)]
pub struct EditParams {
    pub file_path: String,
    pub old_string: String,
    pub new_string: String,
    pub replace_all: bool,
}

/// Result of an Edit rescue attempt
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

/// Parse Edit parameters from a `tool_use` input JSON. Returns `None` when the
/// input is not an Edit (missing `file_path` / `old_string` / `new_string`).
fn parse_edit_params(input: &serde_json::Value) -> Option<EditParams> {
    Some(EditParams {
        file_path: input.get("file_path")?.as_str()?.to_string(),
        old_string: input.get("old_string")?.as_str()?.to_string(),
        new_string: input.get("new_string")?.as_str()?.to_string(),
        replace_all: input.get("replace_all").and_then(|v| v.as_bool()).unwrap_or(false),
    })
}

/// Flatten a `tool_result` content into plain text (for error-message matching).
fn tool_result_text(content: &anthropic::ToolResultContent) -> String {
    match content {
        anthropic::ToolResultContent::Text(s) => s.clone(),
        anthropic::ToolResultContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// Request-side Edit-rescue hook (Issue #88 P6 / Issue #93).
///
/// Both the Edit `tool_use` (file_path/old_string/new_string) and the failing
/// `tool_result` ("String to replace not found") live in the inbound request, so the
/// rescue is stateless: it indexes Edit `tool_use` params by id, then inspects only
/// the **last** message's tool_results (the current turn — replayed history is skipped
/// to avoid redundant rescues and double-counted telemetry). On a unique fuzzy Unicode
/// match, `maybe_rescue_edit` rewrites the file on disk (guarded to `src/` within the
/// cwd) and the failing `tool_result` is rewritten to success so the model does not loop.
pub async fn rescue_request_edits(req: &mut anthropic::AnthropicRequest) {
    use crate::proxy::edit_metrics::{
        classify_edit_error, record_edit_outcome, record_edit_rescue_outcome, EditOutcome,
    };

    // 1. Index Edit tool_use params by id across the whole conversation.
    let mut edits: HashMap<String, EditParams> = HashMap::new();
    for msg in &req.messages {
        if let MessageContent::Blocks(blocks) = &msg.content {
            for b in blocks {
                if let ContentBlock::ToolUse { id, input, .. } = b {
                    if let Some(p) = parse_edit_params(input) {
                        edits.insert(id.clone(), p);
                    }
                }
            }
        }
    }
    if edits.is_empty() {
        return;
    }

    // 2. Only the last message is "new" this turn; skip replayed history.
    let last = match req.messages.last_mut() {
        Some(m) => m,
        None => return,
    };
    let blocks = match &mut last.content {
        MessageContent::Blocks(b) => b,
        MessageContent::Text(_) => return,
    };

    for block in blocks.iter_mut() {
        if let ContentBlock::ToolResult { tool_use_id, content, is_error } = block {
            let id = tool_use_id.clone();
            let params = match edits.get(&id) {
                Some(p) => p,
                None => continue, // not an Edit tool_result
            };

            if *is_error != Some(true) {
                record_edit_outcome(&EditOutcome::Success, &params.file_path, Duration::ZERO);
                continue;
            }

            let err_text = tool_result_text(content);
            let started = Instant::now();
            match maybe_rescue_edit(params, &err_text).await {
                Some(EditRescueResult::Rescued { .. }) => {
                    tracing::info!("Edit rescue: rewrote failing tool_result {} to success", id);
                    *content = anthropic::ToolResultContent::Text(
                        "Edit applied successfully (rescued by NEXUS Unicode fuzzy match)."
                            .to_string(),
                    );
                    *is_error = Some(false);
                    record_edit_outcome(
                        &EditOutcome::Rescued,
                        &params.file_path,
                        started.elapsed(),
                    );
                    record_edit_rescue_outcome("rescued");
                }
                Some(EditRescueResult::NotRescuable { .. }) => {
                    record_edit_outcome(
                        &EditOutcome::Failed(classify_edit_error(&err_text)),
                        &params.file_path,
                        started.elapsed(),
                    );
                    record_edit_rescue_outcome("not_rescuable");
                }
                None => {
                    // An Edit error, but not "String to replace not found" — record only.
                    record_edit_outcome(
                        &EditOutcome::Failed(classify_edit_error(&err_text)),
                        &params.file_path,
                        started.elapsed(),
                    );
                }
            }
        }
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

    // ---- Issue #88 P6 / #93: request-side rescue-hook wiring ----

    fn req(v: serde_json::Value) -> anthropic::AnthropicRequest {
        serde_json::from_value(v).expect("valid AnthropicRequest JSON")
    }

    /// (is_error, flattened text) of the first tool_result in the last message.
    fn last_tool_result(r: &anthropic::AnthropicRequest) -> (Option<bool>, String) {
        match &r.messages.last().expect("at least one message").content {
            MessageContent::Blocks(blocks) => {
                for blk in blocks {
                    if let ContentBlock::ToolResult { is_error, content, .. } = blk {
                        return (*is_error, tool_result_text(content));
                    }
                }
                panic!("no tool_result block in last message");
            }
            MessageContent::Text(_) => panic!("last message is text, not blocks"),
        }
    }

    #[test]
    fn parse_edit_params_extracts_fields_and_rejects_partial() {
        let full = serde_json::json!({
            "file_path": "src/x.rs", "old_string": "a", "new_string": "b", "replace_all": true
        });
        let p = parse_edit_params(&full).expect("full Edit input parses");
        assert_eq!(p.file_path, "src/x.rs");
        assert_eq!(p.old_string, "a");
        assert_eq!(p.new_string, "b");
        assert!(p.replace_all);

        // replace_all defaults to false when omitted.
        let no_flag = serde_json::json!({"file_path":"f","old_string":"a","new_string":"b"});
        assert!(!parse_edit_params(&no_flag).unwrap().replace_all);

        // Missing a required field => None (not an Edit).
        let partial = serde_json::json!({"file_path":"f","old_string":"a"});
        assert!(parse_edit_params(&partial).is_none());
    }

    #[test]
    fn tool_result_text_flattens_text_and_blocks() {
        let t = anthropic::ToolResultContent::Text("hello".to_string());
        assert_eq!(tool_result_text(&t), "hello");

        let b = anthropic::ToolResultContent::Blocks(vec![
            ContentBlock::Text { text: "line1".to_string(), cache_control: None },
            ContentBlock::Text { text: "line2".to_string(), cache_control: None },
        ]);
        assert_eq!(tool_result_text(&b), "line1\nline2");
    }

    #[tokio::test]
    async fn rescue_is_noop_without_edit_tool_use() {
        let mut r = req(serde_json::json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 100,
            "messages": [{ "role": "user", "content": "hello, no tools here" }]
        }));
        let before = serde_json::to_value(&r).unwrap();
        rescue_request_edits(&mut r).await;
        let after = serde_json::to_value(&r).unwrap();
        assert_eq!(before, after, "request without Edit tool_use must be untouched");
    }

    #[tokio::test]
    async fn rescue_preserves_unrescuable_error() {
        // Edit tool_use + a failing tool_result whose error is NOT the rescuable
        // "String to replace not found" => maybe_rescue_edit returns None => preserved.
        let mut r = req(serde_json::json!({
            "model": "m", "max_tokens": 10,
            "messages": [
                { "role": "assistant", "content": [
                    { "type": "tool_use", "id": "ed_1", "name": "Edit",
                      "input": { "file_path": "src/x.rs", "old_string": "a", "new_string": "b" } }
                ]},
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "ed_1",
                      "content": "Some unrelated failure", "is_error": true }
                ]}
            ]
        }));
        rescue_request_edits(&mut r).await;
        let (is_error, text) = last_tool_result(&r);
        assert_eq!(is_error, Some(true), "non-rescuable error must remain an error");
        assert_eq!(text, "Some unrelated failure", "content must be unchanged");
    }

    #[tokio::test]
    async fn rescue_respects_path_guard_outside_src() {
        // Error IS rescuable text, but file_path is outside src/ within cwd =>
        // maybe_rescue_edit returns NotRescuable => is_error stays true (no false rescue).
        let mut r = req(serde_json::json!({
            "model": "m", "max_tokens": 10,
            "messages": [
                { "role": "assistant", "content": [
                    { "type": "tool_use", "id": "ed_2", "name": "Edit",
                      "input": { "file_path": "/tmp/outside.rs", "old_string": "foo", "new_string": "bar" } }
                ]},
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "ed_2",
                      "content": "String to replace not found: foo", "is_error": true }
                ]}
            ]
        }));
        rescue_request_edits(&mut r).await;
        let (is_error, _) = last_tool_result(&r);
        assert_eq!(is_error, Some(true), "path-guard rejection must not flip is_error");
    }

    #[tokio::test]
    async fn rescue_only_processes_last_message() {
        // A rescuable-looking Edit error sits in a NON-last message; the last message
        // is plain text. The hook inspects only the last message, so the historical
        // error must remain untouched (history is never rescued / double-counted).
        let mut r = req(serde_json::json!({
            "model": "m", "max_tokens": 10,
            "messages": [
                { "role": "assistant", "content": [
                    { "type": "tool_use", "id": "ed_3", "name": "Edit",
                      "input": { "file_path": "src/x.rs", "old_string": "foo", "new_string": "bar" } }
                ]},
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "ed_3",
                      "content": "String to replace not found: foo", "is_error": true }
                ]},
                { "role": "assistant", "content": "ok, moving on" }
            ]
        }));
        rescue_request_edits(&mut r).await;
        // Inspect the historical (index 1) tool_result — must still be an error.
        match &r.messages[1].content {
            MessageContent::Blocks(blocks) => match &blocks[0] {
                ContentBlock::ToolResult { is_error, .. } => {
                    assert_eq!(*is_error, Some(true), "historical error must be untouched");
                }
                _ => panic!("expected tool_result"),
            },
            _ => panic!("expected blocks"),
        }
    }
}
