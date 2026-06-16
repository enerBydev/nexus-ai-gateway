use crate::config::Config;
use crate::error::{ProxyError, ProxyResult};
use crate::models::{anthropic, openai};
use crate::prompt_cache::{CacheLocation, PromptCache};
use serde_json::{json, Value};

/// Cache marker extracted from Anthropic request before transformation
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct CacheMarker {
    /// SHA-256 hash of the cache_control-marked content
    pub content_hash: String,
    /// Estimated token count
    pub token_count: u32,
    /// Where the marker was found
    pub location: CacheLocation,
    /// The cache_control value (for logging/debugging)
    pub cache_control_value: serde_json::Value,
}

/// Result of Anthropic -> OpenAI transformation with cache metadata
#[derive(Debug)]
pub struct TransformResult {
    /// The transformed OpenAI request
    pub request: openai::OpenAIRequest,
    /// Which upstream to route to
    pub upstream_name: String,
    /// Cache markers extracted from the request
    pub cache_markers: Vec<CacheMarker>,
}

/// Resolve model name and upstream from model map or config defaults
pub(crate) fn resolve_model_and_upstream(
    req_model: &str,
    has_thinking: bool,
    config: &Config,
) -> (String, String) {
    // 1. Check Model Map first (highest priority)
    if let Some(route) = config.model_map.get(req_model) {
        tracing::info!(
            "[PIN] Model map hit: {} -> {}:{}",
            req_model,
            route.upstream_name,
            route.target_model
        );
        return (route.target_model.clone(), route.upstream_name.clone());
    }
    // 2. Fallback to configured model overrides
    let model =
        if has_thinking { config.reasoning_model.clone() } else { config.completion_model.clone() }
            .unwrap_or_else(|| req_model.to_string());

    tracing::info!("[PIN] Model fallback: {} -> default:{}", req_model, model);
    (model, "default".to_string())
}

/// Transform Anthropic request to OpenAI format
/// Returns (OpenAIRequest, upstream_name) for routing
pub fn anthropic_to_openai(
    req: anthropic::AnthropicRequest,
    config: &Config,
    upstream_name: &str, // Issue #35 F9: for conditional chat_template_kwargs
) -> ProxyResult<TransformResult> {
    // Issue #90-B (ARB Eje A): reasoning activation is now policy-driven (`global_max`),
    // decoupled from the Claude model id and translated to the upstream's mechanism. The
    // default reproduces the prior behavior (force thinking; enable_thinking kwargs for
    // NIM only). Replaces the former hardcoded `has_thinking = true` + inline kwargs.
    let activation =
        crate::reasoning::activation::activate(config.get_upstream_type(upstream_name));
    let has_thinking = activation.has_thinking;

    // Initialize cache markers vector for PHASE 15
    let mut cache_markers: Vec<CacheMarker> = Vec::new();

    // Resolve model AND upstream via model map or config
    let (model, _resolved_upstream) = resolve_model_and_upstream(&req.model, has_thinking, config);

    // Convert messages
    let mut openai_messages = Vec::new();

    // Add system message if present
    // NOTE: Some NIM models (e.g. Qwen3.5) only accept ONE system message.
    // CC sends system as array of blocks -> we consolidate into a single message.
    // PHASE 15: Extract cache markers from system prompts before processing
    if let Some(anthropic::SystemPrompt::Multiple(ref messages)) = req.system {
        for m in messages {
            if let Some(ref cc) = m.cache_control {
                cache_markers.push(CacheMarker {
                    content_hash: PromptCache::hash_content(&m.text),
                    token_count: PromptCache::estimate_tokens(&m.text),
                    location: CacheLocation::SystemPrompt,
                    cache_control_value: cc.clone(),
                });
            }
        }
    }

    if let Some(system) = req.system {
        let system_text = match system {
            anthropic::SystemPrompt::Single(text) => text,
            anthropic::SystemPrompt::Multiple(messages) => messages
                .into_iter()
                .map(|m| {
                    if let Some(ref cc) = m.cache_control {
                        tracing::debug!(
                            target: "nexus::cache",
                            "cache_control in system prompt block (len={}): {:?}",
                            m.text.len(),
                            cc
                        );
                    }
                    m.text
                })
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

    // PHASE 15: Extract cache markers from message content blocks (single pass)
    for msg in &req.messages {
        if let anthropic::MessageContent::Blocks(ref blocks) = msg.content {
            for block in blocks {
                match block {
                    // Direct Text blocks with cache_control
                    anthropic::ContentBlock::Text { ref text, cache_control: Some(ref cc) } => {
                        cache_markers.push(CacheMarker {
                            content_hash: PromptCache::hash_content(text),
                            token_count: PromptCache::estimate_tokens(text),
                            location: CacheLocation::MessageContent,
                            cache_control_value: cc.clone(),
                        });
                    }
                    // Nested Text blocks inside ToolResult -> ToolResultContent::Blocks
                    anthropic::ContentBlock::ToolResult {
                        content: anthropic::ToolResultContent::Blocks(tool_blocks),
                        ..
                    } => {
                        for tool_block in tool_blocks {
                            if let anthropic::ContentBlock::Text {
                                ref text,
                                cache_control: Some(ref cc),
                            } = tool_block
                            {
                                cache_markers.push(CacheMarker {
                                    content_hash: PromptCache::hash_content(text),
                                    token_count: PromptCache::estimate_tokens(text),
                                    location: CacheLocation::MessageContent,
                                    cache_control_value: cc.clone(),
                                });
                            }
                        }
                    }
                    // Other block types don't contain cache_control markers to extract
                    _ => {}
                }
            }
        }
    }

    // Convert user/assistant messages
    for msg in req.messages {
        let converted = convert_message(msg)?;
        openai_messages.extend(converted);
    }

    // Convert tools
    let tools = req.tools.and_then(|tools| {
        let filtered: Vec<_> =
            tools.into_iter().filter(|t| t.tool_type.as_deref() != Some("BatchTool")).collect();

        if filtered.is_empty() {
            None
        } else {
            // Determine the tool format based on upstream type
            let tool_format = match config.get_upstream_type(upstream_name) {
                crate::config::UpstreamType::Anthropic => openai::ToolFormat::Anthropic,
                _ => openai::ToolFormat::OpenAI,
            };

            Some(
                filtered
                    .into_iter()
                    .map(|t| {
                        let parameters = if t.input_schema.is_null() {
                            tracing::debug!(
                                "Tool '{}' has null input_schema, using default schema",
                                t.name
                            );
                            json!({ "type": "object", "properties": {}, "required": [] })
                        } else {
                            ensure_valid_schema(clean_schema(t.input_schema))
                        };

                        openai::ToolSpec {
                            name: t.name,
                            description: t.description,
                            schema: parameters,
                            anthropic_type: if tool_format == openai::ToolFormat::Anthropic {
                                t.tool_type
                            } else {
                                None
                            },
                            tool_format: tool_format.clone(),
                        }
                    })
                    .collect(),
            )
        }
    });

    Ok(TransformResult {
        request: openai::OpenAIRequest {
            model,
            messages: openai_messages,
            max_tokens: Some(req.max_tokens),
            temperature: req.temperature,
            top_p: req.top_p,
            stop: req.stop_sequences,
            stream: req.stream,
            // v6.1: Request token usage in streaming — NIM sends real counts in final chunk
            stream_options: if req.stream == Some(true) {
                Some(json!({"include_usage": true}))
            } else {
                None
            },
            tools,
            tool_choice: None,
            // Issue #35 Bug E / #90-B: chat_template_kwargs is only valid for NIM
            // upstreams; the activation policy (ARB Eje A) resolves it NIM-only.
            chat_template_kwargs: activation.chat_template_kwargs.clone(),
        },
        upstream_name: upstream_name.to_string(),
        cache_markers,
    })
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
                    anthropic::ContentBlock::Text { text, cache_control } => {
                        if let Some(ref cc) = cache_control {
                            tracing::debug!(
                                target: "nexus::cache",
                                "cache_control in content block (len={}): {:?}",
                                text.len(),
                                cc
                            );
                        }
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
                            // Issue #90: idempotent defense-in-depth on the request path.
                            id: crate::tool_id::sanitize_tool_id(&id, 0),
                            call_type: "function".to_string(),
                            function: openai::FunctionCall {
                                name,
                                arguments: serde_json::to_string(&input)
                                    .map_err(ProxyError::Serialization)?,
                            },
                        });
                    }
                    anthropic::ContentBlock::ToolResult { tool_use_id, content, .. } => {
                        let text_content = match content {
                            anthropic::ToolResultContent::Text(s) => s,
                            anthropic::ToolResultContent::Blocks(blocks) => blocks
                                .into_iter()
                                .filter_map(|b| match b {
                                    anthropic::ContentBlock::Text { text, cache_control } => {
                                if let Some(ref cc) = cache_control {
                                    tracing::debug!(
                                        target: "nexus::cache",
                                        "cache_control in ToolResult content block (len={}): {:?}",
                                        text.len(),
                                        cc
                                    );
                                }
                                Some(text)
                            }
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
                            tool_call_id: Some(crate::tool_id::sanitize_tool_id(&tool_use_id, 0)),
                            name: None,
                        });
                    }
                    anthropic::ContentBlock::Thinking { thinking, signature } => {
                        // Phase 8 / Issue #90-B (ARB L5 reconciliation ρ): prior reasoning is
                        // lowered to <previous_reasoning> text for OpenAI/NIM upstreams, which
                        // have no native thinking block. `is_nexus_provenance` distinguishes
                        // NEXUS-synthesized blocks (nexus:v1:) or unsigned ones — always safe to
                        // revert — from real Anthropic signatures. We DROP (never rewrite) the
                        // signature here, so the vercel/ai#9351 overwrite bug cannot occur;
                        // preserving a real signature verbatim is only meaningful for Anthropic
                        // upstreams and is handled on that path, not this OpenAI conversion.
                        if !thinking.is_empty() {
                            let synthetic = signature
                                .as_deref()
                                .map(crate::reasoning::signature::is_nexus_provenance)
                                .unwrap_or(true);
                            if !synthetic {
                                tracing::trace!(
                                    target: "nexus::reasoning",
                                    "real Anthropic thinking lowered to context for OpenAI-compatible upstream"
                                );
                            }
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
                    tool_calls: if tool_calls.is_empty() { None } else { Some(tool_calls) },
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
        // schema is null, empty, or non-object -> replace entirely
        schema = json!({"type": "object", "properties": {}});
    }
    schema
}

/// Transform OpenAI response to Anthropic format
/// Phase 6: Now receives original_model to preserve ClaudeModelID in response
pub fn openai_to_anthropic(
    resp: openai::OpenAIResponse,
    original_model: &str,
    scaling: Option<crate::proxy::token_scaling::TokenScalingParams>,
) -> ProxyResult<anthropic::AnthropicResponse> {
    let choice = resp
        .choices
        .first()
        .ok_or_else(|| ProxyError::Transform("No choices in response".to_string()))?;

    let mut content = Vec::new();

    // Phase 10: Check for reasoning/thinking content from NIM
    // Universal: check reasoning_content first, fall back to reasoning (Kimi K2.5)
    let reasoning_val =
        choice.message.reasoning_content.as_ref().or(choice.message.reasoning.as_ref());
    if let Some(reasoning) = reasoning_val {
        let clean = crate::reasoning::transducer::normalize_full(reasoning);
        if !clean.is_empty() {
            // Issue #90-B (ARB L4): attach a NEXUS provenance signature per the
            // configured mode (default `self`), computed before `clean` is moved.
            let signature = crate::reasoning::signature::reasoning_signature(&clean);
            content.push(anthropic::ResponseContent::Thinking {
                content_type: "thinking".to_string(),
                thinking: clean,
                signature,
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
        for (i, tool_call) in tool_calls.iter().enumerate() {
            let input: Value =
                serde_json::from_str(&tool_call.function.arguments).unwrap_or_else(|_| json!({}));

            content.push(anthropic::ResponseContent::ToolUse {
                content_type: "tool_use".to_string(),
                // Issue #90: sanitize ids like `functions.Bash:0` to Anthropic's ^[A-Za-z0-9_-]+$.
                id: crate::tool_id::sanitize_tool_id(&tool_call.id, i),
                name: tool_call.function.name.clone(),
                input,
            });
        }
    }

    // Phase 4: Detect tool_calls to set stop_reason correctly
    let has_tool_calls =
        choice.message.tool_calls.as_ref().map(|tc| !tc.is_empty()).unwrap_or(false);

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
        usage: {
            let raw_input = resp.usage.prompt_tokens;
            let raw_output = resp.usage.completion_tokens;
            if let Some(params) = scaling {
                let scaled = crate::proxy::token_scaling::scale_token_usage(
                    raw_input,
                    raw_output,
                    params.context_limit,
                    params.cc_context_window,
                    "transform",
                );
                anthropic::Usage {
                    input_tokens: scaled.input,
                    output_tokens: scaled.output,
                    cache_creation_input_tokens: Some(0),
                    cache_read_input_tokens: Some(0),
                    ..Default::default()
                }
            } else {
                anthropic::Usage {
                    input_tokens: raw_input,
                    output_tokens: raw_output,
                    cache_creation_input_tokens: Some(0),
                    cache_read_input_tokens: Some(0),
                    ..Default::default()
                }
            }
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

#[cfg(test)]
#[path = "transform_test.rs"]
mod transform_test;
