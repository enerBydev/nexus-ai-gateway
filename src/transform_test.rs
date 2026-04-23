#[cfg(test)]
mod tests {
    use crate::config::{Config, ModelRoute};
    use crate::models::anthropic::{
        AnthropicRequest, ContentBlock, Message, MessageContent, SystemMessage, SystemPrompt,
    };
    use crate::prompt_cache::CacheLocation;
    use crate::transform::{anthropic_to_openai, sanitize_reasoning, CacheMarker, TransformResult};
    use serde_json::json;
    use std::collections::HashMap;

    /// Helper to create a minimal Config for testing
    fn test_config() -> Config {
        Config {
            port: 8315,
            base_url: "http://localhost:11434".to_string(),
            api_key: None,
            reasoning_model: None,
            completion_model: None,
            debug: false,
            verbose: false,
            web_fetch_enabled: true,
            web_fetch_max_retries: 3,
            web_fetch_timeout_secs: 15,
            upstreams: HashMap::new(),
            model_map: HashMap::new(),
            max_concurrent_per_model: 5,
            permit_timeout_secs: 180,
            upstream_type: crate::config::UpstreamType::NIM,
            prompt_cache_enabled: false,
            prompt_cache_max_entries: 1000,
            prompt_cache_ttl_secs: 300,
        }
    }

    /// Helper to create a Config with model map for testing
    fn test_config_with_model_map() -> Config {
        let mut config = test_config();
        config.model_map.insert(
            "claude-opus-4-6".to_string(),
            ModelRoute {
                upstream_name: "bigmodel".to_string(),
                target_model: "z-ai/glm5".to_string(),
            },
        );
        config
    }

    // =========================================================================
    // PHASE 19: Integration tests for CacheMarker extraction
    // =========================================================================

    #[test]
    fn test_cache_marker_from_system_prompt() {
        // Build an AnthropicRequest with SystemPrompt::Multiple containing
        // a SystemMessage with cache_control
        let system_message = SystemMessage {
            message_type: "text".to_string(),
            text: "This is a system prompt with cache control".to_string(),
            cache_control: Some(json!({"type": "ephemeral"})),
        };

        let req = AnthropicRequest {
            model: "claude-sonnet-4-6".to_string(),
            messages: vec![],
            max_tokens: 4096,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: None,
            tools: None,
            system: Some(SystemPrompt::Multiple(vec![system_message])),
            metadata: None,
            extra: json!({}),
        };

        let config = test_config();
        let result = anthropic_to_openai(req, &config).expect("Transform should succeed");

        // Verify cache_markers has length 1 from system prompt
        assert_eq!(
            result.cache_markers.len(),
            1,
            "Expected 1 cache marker from system prompt, got {}",
            result.cache_markers.len()
        );

        // Verify the marker has SystemPrompt location
        assert_eq!(
            result.cache_markers[0].location,
            CacheLocation::SystemPrompt,
            "Expected SystemPrompt location"
        );

        // Verify content_hash is non-empty
        assert!(
            !result.cache_markers[0].content_hash.is_empty(),
            "content_hash should not be empty"
        );
    }

    #[test]
    fn test_cache_marker_from_content_block() {
        // Build an AnthropicRequest with a message containing ContentBlock::Text
        // with cache_control
        let content_block = ContentBlock::Text {
            text: "Cached message content".to_string(),
            cache_control: Some(json!({"type": "ephemeral"})),
        };

        let message = Message {
            role: "user".to_string(),
            content: MessageContent::Blocks(vec![content_block]),
            extra: json!({}),
        };

        let req = AnthropicRequest {
            model: "claude-sonnet-4-6".to_string(),
            messages: vec![message],
            max_tokens: 4096,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: None,
            tools: None,
            system: None,
            metadata: None,
            extra: json!({}),
        };

        let config = test_config();
        let result = anthropic_to_openai(req, &config).expect("Transform should succeed");

        // Debug output
        eprintln!(
            "DEBUG: cache_markers.len() = {}",
            result.cache_markers.len()
        );
        eprintln!("DEBUG: cache_markers = {:?}", result.cache_markers);

        // Verify cache_markers has at least 1 marker with MessageContent location
        assert!(
            result.cache_markers.len() >= 1,
            "Expected at least 1 cache marker, got {}",
            result.cache_markers.len()
        );

        let has_message_content_marker = result
            .cache_markers
            .iter()
            .any(|m| m.location == CacheLocation::MessageContent);
        assert!(
            has_message_content_marker,
            "Expected at least one marker with MessageContent location"
        );

        // Verify the marker has non-empty content_hash
        for marker in &result.cache_markers {
            assert!(
                !marker.content_hash.is_empty(),
                "content_hash should not be empty"
            );
        }
    }

    #[test]
    fn test_no_cache_markers_without_cache_control() {
        // Build a request without any cache_control fields
        let content_block = ContentBlock::Text {
            text: "Regular message without cache control".to_string(),
            cache_control: None, // No cache control
        };

        let message = Message {
            role: "user".to_string(),
            content: MessageContent::Blocks(vec![content_block]),
            extra: json!({}),
        };

        let req = AnthropicRequest {
            model: "claude-sonnet-4-6".to_string(),
            messages: vec![message],
            max_tokens: 4096,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: None,
            tools: None,
            system: None,
            metadata: None,
            extra: json!({}),
        };

        let config = test_config();
        let result = anthropic_to_openai(req, &config).expect("Transform should succeed");

        // Verify cache_markers is empty
        assert!(
            result.cache_markers.is_empty(),
            "Expected empty cache_markers without cache_control, got {:?}",
            result.cache_markers
        );
    }

    #[test]
    fn test_multiple_cache_markers() {
        // Build a request with both system prompt and message content cache_control
        let system_message = SystemMessage {
            message_type: "text".to_string(),
            text: "System prompt with cache".to_string(),
            cache_control: Some(json!({"type": "ephemeral"})),
        };

        let content_block1 = ContentBlock::Text {
            text: "First cached message".to_string(),
            cache_control: Some(json!({"type": "ephemeral"})),
        };

        let content_block2 = ContentBlock::Text {
            text: "Second cached message".to_string(),
            cache_control: Some(json!({"type": "ephemeral"})),
        };

        let message = Message {
            role: "user".to_string(),
            content: MessageContent::Blocks(vec![content_block1, content_block2]),
            extra: json!({}),
        };

        let req = AnthropicRequest {
            model: "claude-sonnet-4-6".to_string(),
            messages: vec![message],
            max_tokens: 4096,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: None,
            tools: None,
            system: Some(SystemPrompt::Multiple(vec![system_message])),
            metadata: None,
            extra: json!({}),
        };

        let config = test_config();
        let result = anthropic_to_openai(req, &config).expect("Transform should succeed");

        // Verify multiple markers are extracted from message content blocks
        // (system prompt markers not yet implemented)
        assert!(
            result.cache_markers.len() >= 2,
            "Expected at least 2 cache markers from message content blocks, got {}",
            result.cache_markers.len()
        );
    }

    // =========================================================================
    // PHASE 20: Integration tests for TransformResult
    // =========================================================================

    #[test]
    fn test_transform_result_contains_upstream_name() {
        // Create a config with model map
        let config = test_config_with_model_map();

        // Build a request with a model that exists in the map
        let message = Message {
            role: "user".to_string(),
            content: MessageContent::Text("Hello".to_string()),
            extra: json!({}),
        };

        let req = AnthropicRequest {
            model: "claude-opus-4-6".to_string(), // This should map to bigmodel
            messages: vec![message],
            max_tokens: 4096,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: None,
            tools: None,
            system: None,
            metadata: None,
            extra: json!({}),
        };

        let result = anthropic_to_openai(req, &config).expect("Transform should succeed");

        // Verify upstream_name is set to "bigmodel"
        assert_eq!(
            result.upstream_name, "bigmodel",
            "Expected upstream_name to be 'bigmodel', got '{}'",
            result.upstream_name
        );
    }

    #[test]
    fn test_transform_result_request_is_valid() {
        let message = Message {
            role: "user".to_string(),
            content: MessageContent::Text("Hello, world!".to_string()),
            extra: json!({}),
        };

        let req = AnthropicRequest {
            model: "claude-sonnet-4-6".to_string(),
            messages: vec![message],
            max_tokens: 4096,
            temperature: Some(0.7),
            top_p: None,
            top_k: None,
            stop_sequences: Some(vec!["STOP".to_string()]),
            stream: Some(true),
            tools: None,
            system: None,
            metadata: None,
            extra: json!({}),
        };

        let config = test_config();
        let result = anthropic_to_openai(req, &config).expect("Transform should succeed");

        // Verify the request field is a properly constructed OpenAIRequest
        assert!(
            !result.request.model.is_empty(),
            "Request model should not be empty"
        );
        assert!(
            !result.request.messages.is_empty(),
            "Request messages should not be empty"
        );
        assert_eq!(
            result.request.max_tokens,
            Some(4096),
            "max_tokens should be preserved"
        );
        assert_eq!(
            result.request.temperature,
            Some(0.7),
            "temperature should be preserved"
        );
        assert_eq!(
            result.request.stream,
            Some(true),
            "stream should be preserved"
        );
    }

    // =========================================================================
    // Original sanitize_reasoning tests
    // =========================================================================

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
        let input = r#"Let me check that docs file and also look at the service file to see </previous_reasoning><tool_call>Read<arg_key>file</arg_key></tool_call>"#;
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
