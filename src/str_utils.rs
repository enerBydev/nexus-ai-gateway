//! String utilities for safe UTF-8 string truncation.
//!
//! These functions handle multi-byte UTF-8 characters correctly,
//! preventing runtime panics when truncating strings containing
//! non-ASCII characters (e.g., Chinese, Japanese, Korean, emoji).

/// Truncate &str to at most `max_chars` characters, respecting UTF-8 boundaries.
/// Never panics — always returns a valid &str substring.
///
/// # Examples
///
/// ```
/// use nexus_ai_gateway::str_utils::safe_truncate;
///
/// assert_eq!(safe_truncate("hello world", 5), "hello");
/// assert_eq!(safe_truncate("你好世界", 2), "你好"); // 2 chars, not bytes
/// assert_eq!(safe_truncate("🎉🎊🎈🎃", 2), "🎉🎊"); // Emoji are handled correctly
/// ```
pub fn safe_truncate(s: &str, max_chars: usize) -> &str {
    if max_chars == 0 {
        return "";
    }
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

/// Truncate from the end, keeping the last `max_chars` characters.
///
/// # Examples
///
/// ```
/// use nexus_ai_gateway::str_utils::safe_truncate_from_end;
///
/// assert_eq!(safe_truncate_from_end("hello world", 5), "world");
/// assert_eq!(safe_truncate_from_end("你好世界", 2), "世界");
/// ```
pub fn safe_truncate_from_end(s: &str, max_chars: usize) -> &str {
    if max_chars == 0 {
        return "";
    }
    let total = s.chars().count();
    if total <= max_chars {
        return s;
    }
    let start = s
        .char_indices()
        .nth(total - max_chars)
        .map(|(i, _)| i)
        .unwrap_or(0);
    &s[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ascii_truncation() {
        assert_eq!(safe_truncate("hello world", 5), "hello");
    }

    #[test]
    fn test_multibyte_utf8_truncation() {
        // Chinese characters - each is 3 bytes in UTF-8
        let s = "你好世界hello";
        assert_eq!(safe_truncate(s, 2), "你好"); // 2 chars, not bytes
    }

    #[test]
    fn test_emoji_truncation() {
        let s = "🎉🎊🎈🎃";
        assert_eq!(safe_truncate(s, 2), "🎉🎊");
    }

    #[test]
    fn test_mixed_content_truncation() {
        let s = "hello世界🎉";
        assert_eq!(safe_truncate(s, 7), "hello世界");
        assert_eq!(safe_truncate(s, 5), "hello");
        assert_eq!(safe_truncate(s, 8), "hello世界🎉");
    }

    #[test]
    fn test_no_truncation_when_short() {
        assert_eq!(safe_truncate("hi", 10), "hi");
        assert_eq!(safe_truncate("", 10), "");
    }

    #[test]
    fn test_from_end_truncation() {
        assert_eq!(safe_truncate_from_end("hello world", 5), "world");
        assert_eq!(safe_truncate_from_end("你好世界", 2), "世界");
    }

    #[test]
    fn test_from_end_emoji() {
        let s = "🎉🎊🎈🎃";
        assert_eq!(safe_truncate_from_end(s, 2), "🎈🎃");
    }

    #[test]
    fn test_from_end_no_truncation_when_short() {
        assert_eq!(safe_truncate_from_end("hi", 10), "hi");
        assert_eq!(safe_truncate_from_end("", 10), "");
    }

    #[test]
    fn test_from_end_exact_length() {
        let s = "hello";
        assert_eq!(safe_truncate_from_end(s, 5), "hello");
    }

    #[test]
    fn test_zero_max_chars() {
        assert_eq!(safe_truncate("hello", 0), "");
        assert_eq!(safe_truncate_from_end("hello", 0), "");
    }

    #[test]
    fn test_chinese_japanese_korean() {
        // Korean characters are typically 3 bytes each in UTF-8
        let korean = "안녕하세요";
        assert_eq!(safe_truncate(korean, 2), "안녕");

        // Japanese mix of hiragana, katakana, kanji
        let japanese = "こんにちは世界";
        assert_eq!(safe_truncate(japanese, 5), "こんにちは");

        // Mixed CJK with ASCII
        let mixed = "Hello世界안녕";
        assert_eq!(safe_truncate(mixed, 5), "Hello");
        assert_eq!(safe_truncate(mixed, 6), "Hello世");
    }
}
