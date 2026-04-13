use super::*;
use serde_json::json;

#[test]
fn estimate_simple_message() {
    let req = json!({
        "messages": [{"role": "user", "content": "Hello world"}]
    });
    let tokens = estimate_input_tokens(&req);
    assert!(tokens > 0, "Simple message should produce > 0 tokens");
    assert!(
        tokens < 20,
        "Simple message should produce < 20 tokens, got {}",
        tokens
    );
}

#[test]
fn estimate_with_system_prompt() {
    let without_system = json!({
        "messages": [{"role": "user", "content": "Hello world"}]
    });
    let with_system = json!({
        "system": "You are a helpful AI assistant that excels at coding tasks.",
        "messages": [{"role": "user", "content": "Hello world"}]
    });
    let tokens_without = estimate_input_tokens(&without_system);
    let tokens_with = estimate_input_tokens(&with_system);
    assert!(
        tokens_with > tokens_without,
        "System prompt should increase token count: {} vs {}",
        tokens_with,
        tokens_without
    );
}

#[test]
fn estimate_with_tools() {
    let without_tools = json!({
        "messages": [{"role": "user", "content": "Hello"}]
    });
    let with_tools = json!({
        "messages": [{"role": "user", "content": "Hello"}],
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "Read the contents of a file from the filesystem",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string", "description": "Absolute path to the file"}
                        },
                        "required": ["path"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "write_file",
                    "description": "Write content to a file on the filesystem",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string"},
                            "content": {"type": "string"}
                        },
                        "required": ["path", "content"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "run_command",
                    "description": "Execute a shell command and return output",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "command": {"type": "string", "description": "The command to execute"}
                        },
                        "required": ["command"]
                    }
                }
            }
        ]
    });
    let tokens_without = estimate_input_tokens(&without_tools);
    let tokens_with = estimate_input_tokens(&with_tools);
    assert!(
        tokens_with > tokens_without + 50,
        "Tools should add significant tokens: {} vs {} (diff={})",
        tokens_with,
        tokens_without,
        tokens_with - tokens_without
    );
}

#[test]
fn estimate_empty_input() {
    let req = json!({});
    let tokens = estimate_input_tokens(&req);
    assert_eq!(tokens, 1, "Empty input should return minimum 1 token");
}

#[test]
fn estimate_large_context() {
    // ~100K chars of text ≈ ~25K tokens
    let large_text = "a ".repeat(50_000); // 100K chars
    let req = json!({
        "messages": [{"role": "user", "content": large_text}]
    });
    let tokens = estimate_input_tokens(&req);
    assert!(
        tokens > 20_000,
        "100K chars should produce > 20K tokens, got {}",
        tokens
    );
    assert!(
        tokens < 60_000,
        "100K chars should produce < 60K tokens, got {}",
        tokens
    );
}

#[test]
fn estimate_multipart_content() {
    let single = json!({
        "messages": [{"role": "user", "content": "Hello world"}]
    });
    let multi = json!({
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "Hello world"},
                {"type": "text", "text": "This is additional context with more words to count"}
            ]
        }]
    });
    let tokens_single = estimate_input_tokens(&single);
    let tokens_multi = estimate_input_tokens(&multi);
    assert!(
        tokens_multi > tokens_single,
        "Multipart should have more tokens: {} vs {}",
        tokens_multi,
        tokens_single
    );
}

#[test]
fn fallback_produces_nonzero() {
    // Any valid message should always return > 0
    let req = json!({
        "messages": [{"role": "user", "content": "x"}]
    });
    let tokens = estimate_input_tokens(&req);
    assert!(tokens > 0, "Should always produce > 0 tokens");
}

#[test]
fn typed_variant_matches_json_variant() {
    use crate::models::openai::{Message, MessageContent, OpenAIRequest};

    let openai_req = OpenAIRequest {
        model: "test-model".to_string(),
        messages: vec![Message {
            role: "user".to_string(),
            content: Some(MessageContent::Text(
                "What is the capital of France?".to_string(),
            )),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }],
        max_tokens: Some(100),
        temperature: None,
        top_p: None,
        stop: None,
        stream: Some(true),
        stream_options: None,
        tools: None,
        tool_choice: None,
        chat_template_kwargs: None,
    };

    let json_req = serde_json::to_value(&openai_req).unwrap();

    let typed_result = estimate_from_openai_request(&openai_req);
    let json_result = estimate_input_tokens(&json_req);

    assert_eq!(
        typed_result, json_result,
        "Typed and JSON variants should return same result: {} vs {}",
        typed_result, json_result
    );
}

#[test]
fn estimate_system_prompt_array() {
    let req = json!({
        "system": [
            {"type": "text", "text": "You are a coding assistant."},
            {"type": "text", "text": "Always use best practices."}
        ],
        "messages": [{"role": "user", "content": "Hello"}]
    });
    let tokens = estimate_input_tokens(&req);
    assert!(
        tokens > 5,
        "System array should produce meaningful tokens, got {}",
        tokens
    );
}
