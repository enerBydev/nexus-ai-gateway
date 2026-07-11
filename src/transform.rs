use crate::config::Config;
use crate::error::{ProxyError, ProxyResult};
use crate::models::{anthropic, openai};
use serde_json::{json, Value};

/// Result of Anthropic -> OpenAI transformation
#[derive(Debug)]
pub struct TransformResult {
    /// The transformed OpenAI request
    pub request: openai::OpenAIRequest,
    /// Which upstream to route to
    pub upstream_name: String,
}

/// Resolve model name and upstream from model map or config defaults
/// Tier (opus/sonnet/haiku) of a Claude model id (Issue #105). Constrained to real
/// `claude-…-<tier>-…` ids (delimited, lowercased) so a non-Claude model that merely
/// contains a tier word is never rerouted by the family fallback.
fn model_tier(model: &str) -> Option<&'static str> {
    let m = model.to_ascii_lowercase();
    if !m.starts_with("claude-") {
        return None;
    }
    ["opus", "sonnet", "haiku"].into_iter().find(|t| m.contains(&format!("-{t}-")))
}

/// Length of the shared leading byte run between two ids.
fn common_prefix_len(a: &str, b: &str) -> usize {
    a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count()
}

/// Issue #105: find the configured map entry that best covers an unmapped Claude id of the
/// same family. Newer CC builds ship ids (`claude-opus-4-8`, `claude-sonnet-4-5`, ...) the
/// map does not list yet; routing them to the closest same-tier mapping keeps opus/sonnet/
/// haiku usable across CC upgrades without a config change. Picks the same-tier key with the
/// longest shared prefix, breaking ties by the lexicographically largest key (a newer dotted
/// version sorts above an older dated snapshot, e.g. `claude-opus-4-6` > `claude-opus-4-20250115`).
fn best_family_match<'a>(
    req_model: &str,
    map: &'a std::collections::HashMap<String, crate::config::ModelRoute>,
) -> Option<(&'a String, &'a crate::config::ModelRoute)> {
    let tier = model_tier(req_model)?;
    map.iter()
        .filter(|(k, _)| model_tier(k) == Some(tier))
        .max_by_key(|(k, _)| (common_prefix_len(req_model, k), (*k).clone()))
}

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
    // 2. Issue #105: family fallback for newer CC model ids the map does not list yet
    // (e.g. claude-opus-4-8 -> the configured claude-opus-4-6 route). Without this an
    // unmapped id is forwarded verbatim and NVIDIA returns a raw "404 page not found".
    if let Some((matched, route)) = best_family_match(req_model, &config.model_map) {
        tracing::warn!(
            "[PIN] Model family fallback: {} -> {}:{} (no exact map; matched '{}' by family)",
            req_model,
            route.upstream_name,
            route.target_model,
            matched
        );
        return (route.target_model.clone(), route.upstream_name.clone());
    }
    // 3. Fallback to configured model overrides
    let model =
        if has_thinking { config.reasoning_model.clone() } else { config.completion_model.clone() }
            .unwrap_or_else(|| req_model.to_string());

    tracing::info!("[PIN] Model fallback: {} -> default:{}", req_model, model);
    (model, "default".to_string())
}

/// Transform Anthropic request to OpenAI format
/// Returns (OpenAIRequest, upstream_name) for routing
/// Translate CC's `output_config.format` (Anthropic structured-output, json_schema)
/// into an OpenAI `response_format` so NIM / OpenAI-compatible upstreams enforce the
/// schema. CC sends `{ "type": "json_schema", "schema": {...} }`; OpenAI expects
/// `{ "type": "json_schema", "json_schema": { "name", "schema", "strict" } }`.
/// Returns None when the request carries no json_schema format.
fn structured_output_to_response_format(extra: &serde_json::Value) -> Option<serde_json::Value> {
    let format = extra.get("output_config")?.get("format")?;
    if format.get("type")?.as_str()? != "json_schema" {
        return None;
    }
    let schema = format.get("schema")?;
    let name = format.get("name").and_then(|n| n.as_str()).unwrap_or("structured_output");
    Some(json!({
        "type": "json_schema",
        "json_schema": { "name": name, "schema": schema, "strict": true }
    }))
}

pub fn anthropic_to_openai(
    req: anthropic::AnthropicRequest,
    config: &Config,
    upstream_name: &str, // Issue #35 F9: for conditional chat_template_kwargs
) -> ProxyResult<TransformResult> {
    // Issue #58/#101/#57: resolve model FIRST (tiers 1-2) so the target model is known
    // before calling activate(). This breaks the prior circularity where activate() ran
    // before resolve_model_and_upstream(). Tier 3 (no map hit) uses has_thinking=true
    // (global_max default) because no per-model identity is known in that path.
    let upstream_type = config.get_upstream_type(upstream_name);

    // Try tiers 1-2 (model_map exact hit, family fallback) which don't need has_thinking
    let (model, route_mechanism) = if let Some(route) = config.model_map.get(&req.model) {
        tracing::info!(
            "[PIN] Model map hit: {} -> {}:{}",
            req.model,
            route.upstream_name,
            route.target_model
        );
        (route.target_model.clone(), route.thinking_mechanism)
    } else if let Some((matched, route)) = best_family_match(&req.model, &config.model_map) {
        tracing::warn!(
            "[PIN] Model family fallback: {} -> {}:{} (no exact map; matched '{}' by family)",
            req.model,
            route.upstream_name,
            route.target_model,
            matched
        );
        (route.target_model.clone(), route.thinking_mechanism)
    } else {
        // Tier 3: no map hit — use global_max default (has_thinking=true)
        let model = config.reasoning_model.clone().unwrap_or_else(|| req.model.clone());
        tracing::info!("[PIN] Model fallback: {} -> default:{}", req.model, model);
        (model, None)
    };

    // Now activate with the known target model
    let activation = crate::reasoning::activation::activate(
        upstream_type,
        &model,
        config,
        route_mechanism,
        req.extra.get("thinking"),
        req.max_tokens,
    );
    let has_thinking = activation.has_thinking;

    // For the full resolve (needed for upstream_name), use the original function
    let (_model_check, _resolved_upstream) =
        resolve_model_and_upstream(&req.model, has_thinking, config);

    // Convert messages
    let mut openai_messages = Vec::new();

    // Add system message if present
    // NOTE: Some NIM models (e.g. Qwen3.5) only accept ONE system message.
    // CC sends system as array of blocks -> we consolidate into a single message.
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
                            if crate::web_fetch::is_web_fetch_tool(&t.name) {
                                // B3: web_fetch is a server tool with no input_schema. Give the
                                // NIM model a real `url` parameter so it emits {"url":"..."}
                                // instead of an empty {} (which makes URL extraction fail).
                                // NEXUS intercepts the call and executes the fetch inline.
                                tracing::debug!(
                                    "Tool '{}' is web_fetch — synthesizing url input schema",
                                    t.name
                                );
                                json!({
                                    "type": "object",
                                    "properties": {
                                        "url": { "type": "string", "description": "The URL to fetch" }
                                    },
                                    "required": ["url"]
                                })
                            } else {
                                tracing::debug!(
                                    "Tool '{}' has null input_schema, using default schema",
                                    t.name
                                );
                                json!({ "type": "object", "properties": {}, "required": [] })
                            }
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

    // Issue #57 (§8.2): when AnthropicApi thinking is active, force temperature and
    // top_p to None. The Anthropic Messages API requires temperature=1 (or omitted) when
    // thinking is enabled; sending a custom temperature causes a hard 400.
    let (temperature, top_p) =
        if activation.thinking.is_some() { (None, None) } else { (req.temperature, req.top_p) };

    Ok(TransformResult {
        request: openai::OpenAIRequest {
            model,
            messages: openai_messages,
            max_tokens: Some(req.max_tokens),
            temperature,
            top_p,
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
            // Issue #35 Bug E / #90-B / #101: chat_template_kwargs resolved per-model.
            // Only ChatTemplate mechanism produces Some — Mistral/Devstral get None.
            chat_template_kwargs: activation.chat_template_kwargs.clone(),
            // Issue #57: Anthropic-API thinking injection. Only AnthropicApi mechanism
            // produces Some — NIM/OpenAI/OpenRouter get None.
            thinking: activation.thinking.clone(),
            // #126: forward CC's structured-output schema (output_config.format) as an
            // OpenAI response_format so NIM upstreams enforce it instead of returning
            // free-form prose (which made CC's headless verdict fail with "Execution error").
            response_format: structured_output_to_response_format(&req.extra),
        },
        upstream_name: upstream_name.to_string(),
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

/// Build the response content for synthesized NIM reasoning (Issue #90-B F5 · ARB L4):
/// in `durable` mode the reasoning is transported as visible `<thinking>` text (no
/// thinking block — 100% cross-backend safe, never rejected by Anthropic-direct);
/// otherwise it is a thinking block carrying the provenance signature for the mode.
pub(crate) fn reasoning_response_block(
    clean: String,
    mode: crate::reasoning::signature::SignatureMode,
) -> anthropic::ResponseContent {
    use crate::reasoning::signature::SignatureMode;
    if mode == SignatureMode::Durable {
        anthropic::ResponseContent::Text {
            content_type: "text".to_string(),
            text: format!("<thinking>\n{clean}\n</thinking>"),
        }
    } else {
        anthropic::ResponseContent::Thinking {
            signature: crate::reasoning::signature::signature_for_mode(&clean, mode),
            content_type: "thinking".to_string(),
            thinking: clean,
        }
    }
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
            let mode = crate::reasoning::signature::SignatureMode::from_env();
            content.push(reasoning_response_block(clean, mode));
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
            // Issue #119: usage is now Option (degenerate NIM 200s omit it). Default to 0.
            let raw_input = resp.usage.as_ref().map(|u| u.prompt_tokens).unwrap_or(0);
            let raw_output = resp.usage.as_ref().map(|u| u.completion_tokens).unwrap_or(0);
            // Issue #40: extract cached tokens from upstream when available.
            // OpenAI-compatible APIs report prompt_tokens_details.cached_tokens;
            // NIM/others omit it -> defaults to 0 (correct, no caching).
            let raw_cached = resp
                .usage
                .as_ref()
                .and_then(|u| u.prompt_tokens_details.as_ref())
                .map(|d| d.cached_tokens)
                .unwrap_or(0);
            // Non-cached portion: total prompt tokens minus cached tokens.
            // In Anthropic format, input_tokens = fresh (non-cached) tokens only.
            let raw_non_cached = raw_input.saturating_sub(raw_cached);
            if let Some(params) = scaling {
                let scaled = crate::proxy::token_scaling::scale_token_usage(
                    raw_non_cached,
                    raw_output,
                    params.context_limit,
                    params.cc_context_window,
                    "transform",
                );
                let scaled_cache = if raw_cached > 0 {
                    crate::proxy::token_scaling::scale_token_usage(
                        raw_cached,
                        0,
                        params.context_limit,
                        params.cc_context_window,
                        "transform-cache",
                    )
                    .input
                } else {
                    0
                };
                anthropic::Usage {
                    input_tokens: scaled.input,
                    output_tokens: scaled.output,
                    cache_creation_input_tokens: Some(0),
                    cache_read_input_tokens: Some(scaled_cache),
                    ..Default::default()
                }
            } else {
                anthropic::Usage {
                    input_tokens: raw_non_cached,
                    output_tokens: raw_output,
                    cache_creation_input_tokens: Some(0),
                    cache_read_input_tokens: Some(raw_cached),
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
