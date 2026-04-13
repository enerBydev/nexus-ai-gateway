#[cfg(test)]
mod tests {
    use crate::transform::sanitize_reasoning;

    #[test]
    fn test_clean_reasoning_passes_through() {
        let input = "Let me analyze this code structure and understand the architecture.";
        assert_eq!(sanitize_reasoning(input), input);
    }

    #[test]
    fn test_kimi_clean_reasoning() {
        let input = "I need to check the configuration files first, then analyze the proxy logic for any issues.";
        assert_eq!(sanitize_reasoning(input), input);
    }

    #[test]
    fn test_glm5_previous_reasoning_with_tool_calls() {
        let input = r#"Let me check that docs file and also look at the service file to see </previous_reasoning><tool_call>Read<arg_key>file_path</arg_key><arg_value>/home/user/test.md</arg_value></tool_call><tool_call>Bash<arg_key>command</arg_key><arg_value>grep -rn "test" .</arg_value></tool_call>"#;
        let expected = "Let me check that docs file and also look at the service file to see";
        assert_eq!(sanitize_reasoning(input), expected);
    }

    #[test]
    fn test_tool_call_without_previous_reasoning() {
        let input = "Some reasoning text<tool_call>Read<arg_key>file</arg_key></tool_call>";
        let expected = "Some reasoning text";
        assert_eq!(sanitize_reasoning(input), expected);
    }

    #[test]
    fn test_unclosed_tool_call() {
        let input = "Valid reasoning here<tool_call>Read<arg_key>file";
        let expected = "Valid reasoning here";
        assert_eq!(sanitize_reasoning(input), expected);
    }

    #[test]
    fn test_stray_xml_tags() {
        let input = "<previous_reasoning>Some thinking with <arg_key>stray</arg_key> tags";
        let expected = "Some thinking with stray tags";
        assert_eq!(sanitize_reasoning(input), expected);
    }

    #[test]
    fn test_empty_reasoning_after_sanitization() {
        let input = "</previous_reasoning><tool_call>Read</tool_call>";
        assert_eq!(sanitize_reasoning(input), "");
    }

    #[test]
    fn test_empty_input() {
        assert_eq!(sanitize_reasoning(""), "");
    }

    #[test]
    fn test_preserves_normal_angle_brackets() {
        let input = "The value should be x > 5 and y < 10";
        assert_eq!(sanitize_reasoning(input), input);
    }
}
