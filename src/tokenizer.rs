//! Token estimation module using tiktoken cl100k_base tokenizer.
//!
//! Provides ~95% accuracy for modern models (GPT-4, Claude, GLM5, Kimi, Qwen).
//! Falls back to chars/4 (~70% accuracy) if tiktoken fails to initialize.
//!
//! Used in:
//! - `proxy.rs`: inject estimated input_tokens into `message_start` SSE event
//! - `main.rs`: `/v1/messages/count_tokens` endpoint (HTTP API)

use crate::models::openai::OpenAIRequest;

/// Collect all text content from a JSON request body for tokenization.
///
/// Extracts text from:
/// - `system` prompt (string or array of SystemMessage)
/// - `messages[].role` + `messages[].content` (string or parts array)
/// - `tools[]` definitions (serialized JSON)
fn collect_request_text(req: &serde_json::Value) -> String {
    let mut all_text = String::new();

    // Count system prompt
    if let Some(system) = req.get("system") {
        match system {
            serde_json::Value::String(s) => all_text.push_str(s),
            serde_json::Value::Array(arr) => {
                for item in arr {
                    if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                        all_text.push_str(text);
                        all_text.push('\n');
                    }
                }
            }
            _ => all_text.push_str(&system.to_string()),
        }
    }

    // Count messages
    if let Some(serde_json::Value::Array(messages)) = req.get("messages") {
        for msg in messages {
            // Extract role
            if let Some(role) = msg.get("role").and_then(|r| r.as_str()) {
                all_text.push_str(role);
                all_text.push('\n');
            }
            // Extract content (string or array of blocks)
            match msg.get("content") {
                Some(serde_json::Value::String(s)) => {
                    all_text.push_str(s);
                    all_text.push('\n');
                }
                Some(serde_json::Value::Array(blocks)) => {
                    for block in blocks {
                        if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                            all_text.push_str(text);
                            all_text.push('\n');
                        }
                        if let Some(input) = block.get("input") {
                            all_text.push_str(&input.to_string());
                            all_text.push('\n');
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // Count tools (definitions are part of context)
    if let Some(serde_json::Value::Array(tools)) = req.get("tools") {
        for tool in tools {
            all_text.push_str(&tool.to_string());
            all_text.push('\n');
        }
    }

    all_text
}

/// Estimate the number of input tokens in a request body using tiktoken cl100k_base.
///
/// # Arguments
/// * `request_body` - A JSON `Value` representing the request (Anthropic or OpenAI format)
///
/// # Returns
/// Estimated token count (minimum 1). Uses tiktoken cl100k_base for ~95% accuracy,
/// falls back to chars/4 (~70% accuracy) if tiktoken fails.
///
/// # Example
/// ```ignore
/// let req = serde_json::json!({
///     "messages": [{"role": "user", "content": "Hello world"}]
/// });
/// let tokens = estimate_input_tokens(&req);
/// assert!(tokens > 0);
/// ```
pub fn estimate_input_tokens(request_body: &serde_json::Value) -> u32 {
    let all_text = collect_request_text(request_body);

    // Count messages for overhead calculation
    let message_count = request_body
        .get("messages")
        .and_then(|m| m.as_array())
        .map(|a| a.len())
        .unwrap_or(0);

    // Tokenize using tiktoken cl100k_base (GPT-4 tokenizer — closest universal approximation)
    match tiktoken_rs::cl100k_base() {
        Ok(bpe) => {
            let tokens = bpe.encode_with_special_tokens(&all_text);
            // Add ~4 tokens per message for role/formatting overhead (OpenAI convention)
            (tokens.len() + message_count * 4).max(1) as u32
        }
        Err(_) => {
            // Fallback to chars/4 if tokenizer fails to initialize
            tracing::warn!("⚠️ tiktoken init failed, falling back to chars/4 heuristic");
            (all_text.len() / 4).max(1) as u32
        }
    }
}

/// Estimate input tokens from a typed `OpenAIRequest` struct.
///
/// Serializes the request to JSON and delegates to `estimate_input_tokens`.
/// This is the primary entry point used by `proxy.rs` during streaming.
///
/// # Arguments
/// * `req` - The OpenAI request struct (already transformed from Anthropic format)
///
/// # Returns
/// Estimated token count (minimum 1)
pub fn estimate_from_openai_request(req: &OpenAIRequest) -> u32 {
    match serde_json::to_value(req) {
        Ok(val) => estimate_input_tokens(&val),
        Err(_) => {
            tracing::warn!("⚠️ Failed to serialize OpenAIRequest for token estimation");
            1
        }
    }
}

#[cfg(test)]
#[path = "tokenizer_test.rs"]
mod tokenizer_test;
