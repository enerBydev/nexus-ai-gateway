use crate::config::Config;
use crate::error::{ProxyError, ProxyResult};
use crate::models::{anthropic, openai};
use serde_json::{json, Value};

/// Resolve model name and upstream from model map or config defaults
fn resolve_model_and_upstream(
    req_model: &str,
    has_thinking: bool,
    config: &Config,
) -> (String, String) {
    // 1. Check Model Map first (highest priority)
    if let Some(route) = config.model_map.get(req_model) {
        tracing::info!(
            "📍 Model map hit: {} → {}:{}",
            req_model,
            route.upstream_name,
            route.target_model
        );
        return (route.target_model.clone(), route.upstream_name.clone());
    }
    // 2. Fallback to configured model overrides
    let model = if has_thinking {
        config.reasoning_model.clone()
    } else {
        config.completion_model.clone()
    }
    .unwrap_or_else(|| req_model.to_string());

    tracing::info!("📍 Model fallback: {} → default:{}", req_model, model);
    (model, "default".to_string())
}

/// Transform Anthropic request to OpenAI format
/// Returns (OpenAIRequest, upstream_name) for routing
pub fn anthropic_to_openai(
    req: anthropic::AnthropicRequest,
    config: &Config,
) -> ProxyResult<(openai::OpenAIRequest, String)> {
    // v5.0: Force thinking (effort max) for ALL models globally.
    // CC defaults to effort=medium which sends thinking.type="adaptive".
    // NIM models produce better output with enable_thinking=true.
    // This ensures all models (Sonnet, Haiku, GLM4.7, Kimi, Qwen, etc.)
    // receive proper thinking configuration, not just Opus.
    let has_thinking = true;

    // Resolve model AND upstream via model map or config
    let (model, upstream_name) = resolve_model_and_upstream(&req.model, has_thinking, config);

    // Convert messages
    let mut openai_messages = Vec::new();

    // Add system message if present
    // NOTE: Some NIM models (e.g. Qwen3.5) only accept ONE system message.
    // CC sends system as array of blocks → we consolidate into a single message.
    if let Some(system) = req.system {
        let system_text = match system {
            anthropic::SystemPrompt::Single(text) => text,
            anthropic::SystemPrompt::Multiple(messages) => messages
                .into_iter()
                .map(|m| m.text)
                .collect::<Vec<_>>()
                .join("\n\n"),
        };
        openai_messages.push(openai::Message {
            role: "system".to_string(),
            content: Some(openai::MessageContent::Text(system_text)),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        });
    }

    // Convert user/assistant messages
    for msg in req.messages {
        let converted = convert_message(msg)?;
        openai_messages.extend(converted);
    }

    // Convert tools
    let tools = req.tools.and_then(|tools| {
        let filtered: Vec<_> = tools
            .into_iter()
            .filter(|t| t.tool_type.as_deref() != Some("BatchTool"))
            .collect();

        if filtered.is_empty() {
            None
        } else {
            Some(
                filtered
                    .into_iter()
                    .map(|t| openai::Tool {
                        tool_type: "function".to_string(),
                        function: openai::Function {
                            name: t.name,
                            description: t.description,
                            parameters: ensure_valid_schema(clean_schema(t.input_schema)),
                        },
                    })
                    .collect(),
            )
        }
    });

    Ok((
        openai::OpenAIRequest {
            model,
            messages: openai_messages,
            max_tokens: Some(req.max_tokens),
            temperature: req.temperature,
            top_p: req.top_p,
            stop: req.stop_sequences,
            stream: req.stream,
            tools,
            tool_choice: None,
            chat_template_kwargs: if has_thinking {
                Some(json!({
                    "enable_thinking": true,
                    "clear_thinking": false
                }))
            } else {
                None
            },
        },
        upstream_name,
    ))
}

/// Convert a single Anthropic message to one or more OpenAI messages
fn convert_message(msg: anthropic::Message) -> ProxyResult<Vec<openai::Message>> {
    let mut result = Vec::new();

    match msg.content {
        anthropic::MessageContent::Text(text) => {
            result.push(openai::Message {
                role: msg.role,
                content: Some(openai::MessageContent::Text(text)),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            });
        }
        anthropic::MessageContent::Blocks(blocks) => {
            let mut current_content_parts = Vec::new();
            let mut tool_calls = Vec::new();

            for block in blocks {
                match block {
                    anthropic::ContentBlock::Text { text, .. } => {
                        current_content_parts.push(openai::ContentPart::Text { text });
                    }
                    anthropic::ContentBlock::Image { source } => {
                        let data_url = format!("data:{};base64,{}", source.media_type, source.data);
                        current_content_parts.push(openai::ContentPart::ImageUrl {
                            image_url: openai::ImageUrl { url: data_url },
                        });
                    }
                    anthropic::ContentBlock::ToolUse { id, name, input } => {
                        tool_calls.push(openai::ToolCall {
                            id,
                            call_type: "function".to_string(),
                            function: openai::FunctionCall {
                                name,
                                arguments: serde_json::to_string(&input)
                                    .map_err(ProxyError::Serialization)?,
                            },
                        });
                    }
                    anthropic::ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => {
                        let text_content = match content {
                            anthropic::ToolResultContent::Text(s) => s,
                            anthropic::ToolResultContent::Blocks(blocks) => blocks
                                .into_iter()
                                .filter_map(|b| match b {
                                    anthropic::ContentBlock::Text { text, .. } => Some(text),
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join("\n"),
                        };
                        // Tool results become separate messages with role "tool"
                        result.push(openai::Message {
                            role: "tool".to_string(),
                            content: Some(openai::MessageContent::Text(text_content)),
                            tool_calls: None,
                            tool_call_id: Some(tool_use_id),
                            name: None,
                        });
                    }
                    anthropic::ContentBlock::Thinking { thinking, .. } => {
                        // Phase 8: Preserve thinking as context for the model
                        if !thinking.is_empty() {
                            current_content_parts.push(openai::ContentPart::Text {
                                text: format!(
                                    "<previous_reasoning>\n{}\n</previous_reasoning>",
                                    thinking
                                ),
                            });
                        }
                    }
                    anthropic::ContentBlock::Unknown => {
                        // Skip unknown/future block types silently
                    }
                }
            }

            // Add message with content and/or tool calls
            if !current_content_parts.is_empty() || !tool_calls.is_empty() {
                let content = if current_content_parts.is_empty() {
                    None
                } else if current_content_parts.len() == 1 {
                    match &current_content_parts[0] {
                        openai::ContentPart::Text { text } => {
                            Some(openai::MessageContent::Text(text.clone()))
                        }
                        _ => Some(openai::MessageContent::Parts(current_content_parts)),
                    }
                } else {
                    Some(openai::MessageContent::Parts(current_content_parts))
                };

                result.push(openai::Message {
                    role: msg.role,
                    content,
                    tool_calls: if tool_calls.is_empty() {
                        None
                    } else {
                        Some(tool_calls)
                    },
                    tool_call_id: None,
                    name: None,
                });
            }
        }
    }

    Ok(result)
}

/// Clean JSON schema by removing unsupported formats
fn clean_schema(mut schema: Value) -> Value {
    if let Some(obj) = schema.as_object_mut() {
        // Remove "format": "uri"
        if obj.get("format").and_then(|v| v.as_str()) == Some("uri") {
            obj.remove("format");
        }

        // Recursively clean nested schemas
        if let Some(properties) = obj.get_mut("properties").and_then(|v| v.as_object_mut()) {
            for (_, value) in properties.iter_mut() {
                *value = clean_schema(value.clone());
            }
        }

        if let Some(items) = obj.get_mut("items") {
            *items = clean_schema(items.clone());
        }
    }

    schema
}

/// Ensure schema has minimum required fields for OpenAI/NIM compatibility
/// Prevents 422 errors when CC sends tools with empty/minimal input_schema (e.g. WebSearch)
fn ensure_valid_schema(mut schema: Value) -> Value {
    if let Some(obj) = schema.as_object_mut() {
        if !obj.contains_key("type") {
            obj.insert("type".to_string(), json!("object"));
        }
        if !obj.contains_key("properties") {
            obj.insert("properties".to_string(), json!({}));
        }
    } else {
        // schema is null, empty, or non-object → replace entirely
        schema = json!({"type": "object", "properties": {}});
    }
    schema
}

/// Transform OpenAI response to Anthropic format
/// Phase 6: Now receives original_model to preserve ClaudeModelID in response
pub fn openai_to_anthropic(
    resp: openai::OpenAIResponse,
    original_model: &str,
) -> ProxyResult<anthropic::AnthropicResponse> {
    let choice = resp
        .choices
        .first()
        .ok_or_else(|| ProxyError::Transform("No choices in response".to_string()))?;

    let mut content = Vec::new();

    // Phase 10: Check for reasoning/thinking content from NIM
    // Universal: check reasoning_content first, fall back to reasoning (Kimi K2.5)
    let reasoning_val = choice
        .message
        .reasoning_content
        .as_ref()
        .or(choice.message.reasoning.as_ref());
    if let Some(reasoning) = reasoning_val {
        if !reasoning.is_empty() {
            content.push(anthropic::ResponseContent::Thinking {
                content_type: "thinking".to_string(),
                thinking: reasoning.clone(),
            });
        }
    }

    // Add text content if present
    if let Some(text) = &choice.message.content {
        if !text.is_empty() {
            content.push(anthropic::ResponseContent::Text {
                content_type: "text".to_string(),
                text: text.clone(),
            });
        }
    }

    // Add tool calls if present
    if let Some(tool_calls) = &choice.message.tool_calls {
        for tool_call in tool_calls {
            let input: Value =
                serde_json::from_str(&tool_call.function.arguments).unwrap_or_else(|_| json!({}));

            content.push(anthropic::ResponseContent::ToolUse {
                content_type: "tool_use".to_string(),
                id: tool_call.id.clone(),
                name: tool_call.function.name.clone(),
                input,
            });
        }
    }

    // Phase 4: Detect tool_calls to set stop_reason correctly
    let has_tool_calls = choice
        .message
        .tool_calls
        .as_ref()
        .map(|tc| !tc.is_empty())
        .unwrap_or(false);

    let stop_reason = if has_tool_calls {
        Some("tool_use".to_string())
    } else {
        choice
            .finish_reason
            .as_ref()
            .map(|r| match r.as_str() {
                "tool_calls" => "end_turn",
                "stop" => "end_turn",
                "length" => "max_tokens",
                _ => "end_turn",
            })
            .map(String::from)
    };

    Ok(anthropic::AnthropicResponse {
        id: resp.id,
        response_type: "message".to_string(),
        role: "assistant".to_string(),
        content,
        model: original_model.to_string(), // Phase 6: preserve original ClaudeModelID
        stop_reason,
        stop_sequence: None,
        usage: anthropic::Usage {
            input_tokens: resp.usage.prompt_tokens,
            output_tokens: resp.usage.completion_tokens,
        },
    })
}

/// Map OpenAI finish reason to Anthropic stop reason
/// Phase 5: Added has_tool_calls parameter for tool detection
pub fn map_stop_reason(finish_reason: Option<&str>, has_tool_calls: bool) -> Option<String> {
    if has_tool_calls {
        return Some("tool_use".to_string());
    }
    finish_reason.map(|r| {
        match r {
            "tool_calls" => "end_turn",
            "stop" => "end_turn",
            "length" => "max_tokens",
            _ => "end_turn",
        }
        .to_string()
    })
}
