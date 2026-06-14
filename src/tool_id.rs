//! Sanitization of upstream tool-call ids into the charset the Anthropic API accepts.
//!
//! Anthropic validates every `tool_use.id` (and `tool_result.tool_use_id`) against
//! `^[A-Za-z0-9_-]+$` (the base64url alphabet, |V| = 64). Some OpenAI-compatible
//! upstreams (NIM / vLLM) emit ids such as `functions.<tool>:<idx>` (e.g.
//! `functions.Bash:0`) which contain `.` and `:`. Claude Code stores those ids in
//! its transcript and replays them; when a conversation is later opened against the
//! Anthropic-direct backend, the strict server-side validator rejects the whole
//! request with HTTP 400. NEXUS therefore sanitizes every id it emits to Claude Code.
//!
//! See `docs/Investigacion-tool-use-id-cross-backend/` (Issue #90, Part A).

/// `true` if `s` already matches `^[A-Za-z0-9_-]+$` (non-empty, valid charset only).
#[inline]
fn is_valid_tool_id(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Returns a tool-call id guaranteed to satisfy Anthropic's `^[A-Za-z0-9_-]+$`.
///
/// Properties (see entregable 03): deterministic, idempotent, length-preserving for
/// non-empty inputs, and injective over the real `functions.<tool>:<idx>` format.
///
/// - If `raw` is already valid, it is returned unchanged (the ~99.86% fast path).
/// - Otherwise every byte outside `[A-Za-z0-9_-]` is replaced by `_`.
/// - If `raw` is empty, a deterministic `toolu_<fallback_index>` is generated so the
///   `+` (non-empty) rule holds. `fallback_index` is only consulted for empty input.
pub fn sanitize_tool_id(raw: &str, fallback_index: usize) -> String {
    if is_valid_tool_id(raw) {
        return raw.to_string();
    }
    if raw.is_empty() {
        return format!("toolu_{fallback_index}");
    }
    raw.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;

    fn valid(s: &str) -> bool {
        Regex::new(r"^[A-Za-z0-9_-]+$").unwrap().is_match(s)
    }

    #[test]
    fn passthrough_valid_openai_id() {
        assert_eq!(sanitize_tool_id("call_abc123XYZ", 0), "call_abc123XYZ");
    }

    #[test]
    fn passthrough_valid_nim_id() {
        assert_eq!(
            sanitize_tool_id("chatcmpl-tool-81c39bc66a7f5fe7", 0),
            "chatcmpl-tool-81c39bc66a7f5fe7"
        );
    }

    #[test]
    fn passthrough_valid_anthropic_id() {
        assert_eq!(sanitize_tool_id("toolu_01ABCdef", 0), "toolu_01ABCdef");
    }

    #[test]
    fn replaces_dot_and_colon() {
        assert_eq!(sanitize_tool_id("functions.Bash:0", 0), "functions_Bash_0");
        assert_eq!(sanitize_tool_id("functions.Read:18", 0), "functions_Read_18");
    }

    #[test]
    fn handles_all_29_real_violators() {
        // The 29 unique invalid ids observed in real transcripts were all of the
        // form functions.<ToolName>:<index>. Each must become valid.
        let names = ["Agent", "Bash", "Read"];
        for name in names {
            for idx in 0..25 {
                let raw = format!("functions.{name}:{idx}");
                let out = sanitize_tool_id(&raw, 0);
                assert!(valid(&out), "sanitized id must be valid: {raw} -> {out}");
                assert!(!out.contains('.') && !out.contains(':'));
            }
        }
    }

    #[test]
    fn empty_id_gets_fallback() {
        assert_eq!(sanitize_tool_id("", 3), "toolu_3");
        assert!(valid(&sanitize_tool_id("", 0)));
    }

    #[test]
    fn output_always_matches_pattern() {
        // Property (R1): for arbitrary inputs the output is always accepted.
        let cases = [
            "functions.Bash:0",
            "a.b:c/d+e=f",
            "tool\u{2192}x",
            "with space",
            "",
            "UPPER_lower-123",
            "::::",
            ".",
        ];
        for (i, c) in cases.iter().enumerate() {
            assert!(valid(&sanitize_tool_id(c, i)), "failed for {c:?}");
        }
    }

    #[test]
    fn is_idempotent() {
        for c in ["functions.Bash:0", "call_abc", "", "a:b.c"] {
            let once = sanitize_tool_id(c, 1);
            let twice = sanitize_tool_id(&once, 1);
            assert_eq!(once, twice, "S must be idempotent for {c:?}");
        }
    }

    #[test]
    fn preserves_length_for_nonempty() {
        // Length-preserving for non-empty inputs => never exceeds an already-accepted length.
        for c in ["functions.Bash:0", "a.b:c", "call_x"] {
            assert_eq!(sanitize_tool_id(c, 0).len(), c.len(), "length must be preserved for {c:?}");
        }
    }

    #[test]
    fn pairing_invariant_preserved() {
        // tool_use.id and tool_result.tool_use_id carry the same string; a deterministic
        // function maps both identically, preserving the pairing.
        let id = "functions.Bash:7";
        assert_eq!(sanitize_tool_id(id, 0), sanitize_tool_id(id, 99));
    }

    #[test]
    fn non_ascii_replaced() {
        let out = sanitize_tool_id("tool\u{2192}id", 0);
        assert_eq!(out, "tool_id");
        assert!(valid(&out));
    }

    #[test]
    fn injective_on_real_format() {
        // Distinct functions.<tool>:<idx> ids never collide after sanitization.
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for name in ["Bash", "Read", "Agent"] {
            for idx in 0..25 {
                let out = sanitize_tool_id(&format!("functions.{name}:{idx}"), 0);
                assert!(seen.insert(out.clone()), "collision on {out}");
            }
        }
    }
}
