use std::sync::Arc;

use axum::response::IntoResponse;
use axum::Json;
use reqwest::Client;

use crate::config::Config;
use crate::error::{ProxyError, ProxyResult};
use crate::models::{anthropic, openai};
use crate::proxy::concurrency::{acquire_model_permit, ModelSemaphores};
use crate::proxy::retry::resilient_send;
use crate::transform;
use crate::web_fetch;

/// Search for web_fetch tool_use in an Anthropic response
pub(crate) fn find_web_fetch_in_response(
    resp: &anthropic::AnthropicResponse,
) -> Option<(String, String, serde_json::Value)> {
    for content in &resp.content {
        if let anthropic::ResponseContent::ToolUse { id, name, input, .. } = content {
            if web_fetch::is_web_fetch_tool(name) {
                return Some((id.clone(), name.clone(), input.clone()));
            }
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_non_streaming(
    config: Arc<Config>,
    client: Client,
    openai_req: openai::OpenAIRequest,
    original_req: anthropic::AnthropicRequest,
    upstream_name: &str,
    model_semaphores: ModelSemaphores,
    circuit_breaker: &crate::proxy::concurrency::CircuitBreaker,
    context_limit: u32, // FIX 6: for token scaling
) -> ProxyResult<axum::response::Response> {
    // ╔═══════════════════════════════════════════╗
    // ║ Concurrency Shield: acquire model permit ║
    // ╚═══════════════════════════════════════════╝
    let _permit = acquire_model_permit(
        &model_semaphores,
        &openai_req.model,
        config.max_concurrent_per_model,
        config.permit_timeout_secs,
    )
    .await?;

    tracing::debug!(
        "Sending non-streaming request to {} (upstream: {})",
        config.get_upstream_url(upstream_name),
        upstream_name
    );
    tracing::debug!("Request model: {}", openai_req.model);

    // State for web_fetch interception loop
    let mut current_openai_req = openai_req;
    let mut current_messages = original_req.messages.clone();
    let mut fetch_count: u32 = 0;

    loop {
        // === Resilient send with auto-retry on 429/400 ===
        let openai_resp = resilient_send(
            &client,
            &config,
            &mut current_openai_req,
            upstream_name,
            circuit_breaker,
        )
        .await?;

        if config.verbose {
            tracing::trace!(
                "Received OpenAI response: {}",
                serde_json::to_string_pretty(&openai_resp).unwrap_or_default()
            );
        }

        let anthropic_resp = transform::openai_to_anthropic(openai_resp, &original_req.model)?;

        // FIX 2: Check if context is nearly full after successful retry
        let cc_context_window: u32 =
            std::env::var("CC_CONTEXT_WINDOW").ok().and_then(|v| v.parse().ok()).unwrap_or(200_000);

        // FIX 6: Token scaling for non-streaming path (parity with streaming)
        let scale_tokens = |real_tokens: u32| -> u32 {
            if context_limit > 0 {
                if context_limit < cc_context_window {
                    let scaled = (real_tokens as f64 * cc_context_window as f64
                        / context_limit as f64) as u32;
                    tracing::debug!(
                        "📐 [non-streaming] Scaling up input_tokens: {} → {} (upstream_ctx={}K < cc_ctx={}K)",
                        real_tokens,
                        scaled,
                        context_limit / 1000,
                        cc_context_window / 1000
                    );
                    scaled
                } else {
                    let scaled = (real_tokens as f64 * 1.1) as u32;
                    tracing::debug!(
                        "📐 [non-streaming] Scaling up input_tokens: {} → {} (upstream_ctx={}K > cc_ctx={}K)",
                        real_tokens,
                        scaled,
                        context_limit / 1000,
                        cc_context_window / 1000
                    );
                    scaled.min(cc_context_window)
                }
            } else {
                real_tokens
            }
        };

        let input_tokens = anthropic_resp.usage.input_tokens as u32;
        let scaled_input_tokens = scale_tokens(input_tokens); // FIX 6: Apply scaling
        let context_threshold_pct = crate::proxy::get_overflow_threshold_pct();
        let context_threshold = cc_context_window * context_threshold_pct / 100;
        if scaled_input_tokens > context_threshold {
            tracing::warn!(
                "⚠️ Context nearly full ({} scaled tokens = {}% of {}K, threshold={}%) — returning ContextOverflow",
                scaled_input_tokens,
                scaled_input_tokens * 100 / cc_context_window,
                cc_context_window / 1000,
                context_threshold_pct
            );
            return Err(ProxyError::ContextOverflow(format!(
                "Context window {}% full ({}/{}, threshold={}%). Use /compact to reduce context.",
                scaled_input_tokens * 100 / cc_context_window,
                scaled_input_tokens / 1000,
                cc_context_window / 1000,
                context_threshold_pct
            )));
        }

        if config.verbose {
            tracing::trace!(
                "Transformed Anthropic response: {}",
                serde_json::to_string_pretty(&anthropic_resp).unwrap_or_default()
            );
        }

        // === WebFetch Interception ===
        if config.web_fetch_enabled {
            if let Some((tool_id, tool_name, input)) = find_web_fetch_in_response(&anthropic_resp) {
                fetch_count += 1;
                if fetch_count > config.web_fetch_max_retries {
                    tracing::warn!(
                        "[WebFetch] Max retries ({}) reached, returning as-is",
                        config.web_fetch_max_retries
                    );
                    return Ok(Json(anthropic_resp).into_response());
                }

                let fetch_url = web_fetch::extract_url(&input)
                    .ok_or_else(|| ProxyError::WebFetch("No URL in web_fetch input".into()))?;

                let content = web_fetch::execute_fetch(&client, &fetch_url, &config)
                    .await
                    .unwrap_or_else(|e| format!("Error fetching {}: {}", fetch_url, e));

                tracing::info!(
                    "[WebFetch] Fetch #{} complete: {} chars from {}",
                    fetch_count,
                    content.len(),
                    fetch_url
                );

                // Build assistant message with the tool_use
                let assistant_tool_use = anthropic::Message {
                    role: "assistant".to_string(),
                    content: anthropic::MessageContent::Blocks(vec![
                        anthropic::ContentBlock::ToolUse {
                            id: tool_id.clone(),
                            name: tool_name,
                            input,
                        },
                    ]),
                    extra: serde_json::json!({}),
                };

                // Build user message with tool_result
                let user_tool_result = anthropic::Message {
                    role: "user".to_string(),
                    content: anthropic::MessageContent::Blocks(vec![
                        anthropic::ContentBlock::ToolResult {
                            tool_use_id: tool_id,
                            content: anthropic::ToolResultContent::Text(content),
                            is_error: None,
                        },
                    ]),
                    extra: serde_json::json!({}),
                };

                // Append to messages and rebuild request
                current_messages.push(assistant_tool_use);
                current_messages.push(user_tool_result);

                let mut rebuilt_req = original_req.clone();
                rebuilt_req.messages = current_messages.clone();
                let transform_result = transform::anthropic_to_openai(rebuilt_req, &config)?;
                current_openai_req = transform_result.request;

                tracing::info!(
                    "[WebFetch] Re-sending to NIM with tool_result (attempt #{})",
                    fetch_count
                );
                continue;
            }
        }

        return Ok(Json(anthropic_resp).into_response());
    }
}
