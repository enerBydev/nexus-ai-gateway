use std::sync::Arc;
use std::time::Duration;

use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures::stream::{Stream, StreamExt};
use reqwest::Client;
use serde_json::json;

use crate::config::Config;
use crate::error::ProxyResult;
use crate::models::{anthropic, openai};
use crate::proxy::concurrency::{acquire_model_permit, ModelSemaphores};
use crate::proxy::retry::{resilient_send_raw, CHUNK_TIMEOUT_SECS, MAX_SSE_BUFFER};
use crate::tokenizer;
use crate::transform;
use crate::web_fetch;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_streaming(
    config: Arc<Config>,
    client: Client,
    openai_req: openai::OpenAIRequest,
    upstream_name: &str,
    original_model: &str,
    model_semaphores: ModelSemaphores,
    calibration: tokenizer::CalibrationFactors,
    precomputed_estimate: u32,
    context_limit: u32,
    cc_context_window: u32, // Issue #28: resolved dynamically
    _circuit_breaker: &crate::proxy::concurrency::CircuitBreaker,
) -> ProxyResult<Response> {
    let permit = acquire_model_permit(
        &model_semaphores,
        &openai_req.model,
        config.max_concurrent_per_model,
        config.permit_timeout_secs,
    )
    .await?;

    let url = config.get_upstream_url(upstream_name);
    tracing::debug!("Sending streaming request to {} (upstream: {})", url, upstream_name);
    tracing::debug!("Request model: {}", openai_req.model);

    let mut mutable_req = openai_req;
    let raw_estimate = precomputed_estimate;
    let nim_model_name = mutable_req.model.clone();
    let calibrated_estimate = calibration.apply(&nim_model_name, raw_estimate);
    tracing::debug!(
        "🎯 Token estimate: raw={}, calibrated={} (factor={:.4}, model={})",
        raw_estimate,
        calibrated_estimate,
        calibration.get(&nim_model_name),
        nim_model_name
    );

    let response =
        resilient_send_raw(&client, &config, &mut mutable_req, upstream_name, _circuit_breaker)
            .await?;

    let stream = response.bytes_stream();
    let original_model_owned = original_model.to_string();
    let sse_stream = create_sse_stream(
        stream,
        original_model_owned,
        permit,
        calibrated_estimate,
        raw_estimate,
        nim_model_name,
        calibration,
        context_limit,
        cc_context_window, // Issue #28: resolved dynamically
        _circuit_breaker,
    );

    let mut headers = HeaderMap::new();
    headers.insert("Content-Type", HeaderValue::from_static("text/event-stream"));
    headers.insert("Cache-Control", HeaderValue::from_static("no-cache"));
    headers.insert("Connection", HeaderValue::from_static("keep-alive"));

    Ok((headers, Body::from_stream(sse_stream)).into_response())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn create_sse_stream(
    stream: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    original_model: String,
    _permit: tokio::sync::OwnedSemaphorePermit,
    estimated_input_tokens: u32,
    raw_tiktoken_estimate: u32,
    nim_model_name: String,
    calibration: tokenizer::CalibrationFactors,
    context_limit: u32,
    cc_context_window: u32, // Issue #28: resolved dynamically
    _circuit_breaker: &crate::proxy::concurrency::CircuitBreaker,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
    async_stream::stream! {
        tracing::debug!(
            "📐 Auto-compact scaling: cc_context_window={}K, model_context={}K",
            cc_context_window / 1000,
            context_limit / 1000
        );
        let scale_tokens = |real_tokens: u32| -> u32 {
            if context_limit > 0 {
                if context_limit < cc_context_window {
                    // Upstream has LESS context than CC — inflate so CC sees approaching limit
                    let scaled = (real_tokens as f64 * cc_context_window as f64
                        / context_limit as f64) as u32;
                    tracing::debug!(
                        "📐 Scaling up input_tokens: {} → {} (upstream_ctx={}K < cc_ctx={}K)",
                        real_tokens,
                        scaled,
                        context_limit / 1000,
                        cc_context_window / 1000
                    );
                    scaled
                } else {
                    // Upstream has MORE context than CC — CC will hit its limit first
                    // Add 10% buffer so CC auto-compacts before hitting the hard limit
                    let fill_ratio = real_tokens as f64 / cc_context_window as f64;
                    let scaled = (real_tokens as f64 * 1.1) as u32;
                    tracing::debug!(
                        "📐 Scaling up input_tokens: {} → {} (fill={:.1}%, upstream_ctx={}K > cc_ctx={}K)",
                        real_tokens,
                        scaled,
                        fill_ratio * 100.0,
                        context_limit / 1000,
                        cc_context_window / 1000
                    );
                    scaled.min(cc_context_window) // Never exceed CC's window
                }
            } else {
                real_tokens
            }
        };

        let _ = &_permit;

        let mut buffer = String::with_capacity(8192);
        let mut message_id = None;
        let mut current_model = None;
        let mut content_index = 0;
        let mut tool_call_id = None;
        let mut _tool_call_name: Option<String> = None;
        let mut tool_call_args = String::new();
        let mut has_sent_message_start = false;
        let mut current_block_type: Option<String> = None;
        let original_model_owned = original_model;
        let mut tool_calls_emitted = false;
        let mut is_intercepting_fetch = false;
        let mut _fetch_tool_name: Option<String> = None;
        let mut _fetch_tool_id_buf: Option<String> = None;
        let mut fetch_args_buffer = String::new();
        let mut suppressed_block_start = false;
        let mut accumulated_input_tokens: u32 = estimated_input_tokens;
        let mut accumulated_output_tokens: u32 = 0;
        let mut saved_stop_reason: Option<String> = None;
        let reasoning_poisoned = false;

        tokio::pin!(stream);

        let chunk_timeout = Duration::from_secs(CHUNK_TIMEOUT_SECS);
        loop {
            let chunk = match tokio::time::timeout(chunk_timeout, stream.next()).await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(_) => {
                    tracing::error!("⏰ Stream chunk timeout after {}s — emitting error event", CHUNK_TIMEOUT_SECS);
                    if has_sent_message_start {
                        // Close any open content blocks
                        if current_block_type.is_some() {
                            let stop_block = json!({"type": "content_block_stop", "index": content_index});
                            yield Ok(Bytes::from(format!("event: content_block_stop\ndata: {}\n\n",
                                serde_json::to_string(&stop_block).unwrap_or_default())));
                        }
                        // Emit error event instead of synthetic success
                        // CC treats api_error as retryable — will retry the request
                        let error_event = json!({
                            "type": "error",
                            "error": {
                                "type": "api_error",
                                "message": format!(
                                    "Stream timeout: upstream stopped responding after {}s. The response may be incomplete.",
                                    CHUNK_TIMEOUT_SECS
                                )
                            }
                        });
                        yield Ok(Bytes::from(format!("event: error\ndata: {}\n\n",
                            serde_json::to_string(&error_event).unwrap_or_default())));
                        // Emit message_stop to cleanly terminate the stream
                        let stop_event = json!({"type": "message_stop"});
                        yield Ok(Bytes::from(format!("event: message_stop\ndata: {}\n\n",
                            serde_json::to_string(&stop_event).unwrap_or_default())));
                    }
                    break;
                }
            };

            match chunk {
                Ok(bytes) => {
                    let text = String::from_utf8_lossy(&bytes);
                    buffer.push_str(&text);

                    if buffer.len() > MAX_SSE_BUFFER {
                        tracing::error!("⛔ SSE buffer overflow: {} bytes exceeds {}MB limit — aborting stream",
                            buffer.len(), MAX_SSE_BUFFER / 1024 / 1024);
                        break;
                    }

                    while let Some(pos) = buffer.find("\n\n") {
                        let line = buffer[..pos].to_string();
                        buffer = buffer[pos + 2..].to_string();

                        if line.trim().is_empty() {
                            continue;
                        }

                        for l in line.lines() {
                            if let Some(data) = l.strip_prefix("data: ") {
                                if data.trim() == "[DONE]" {
                                    if let Some(ref stop) = saved_stop_reason {
                                        let delta_event = json!({
                                            "type": "message_delta",
                                            "delta": {
                                                "stop_reason": stop,
                                                "stop_sequence": serde_json::Value::Null
                                            },
                                            "usage": {
                                                "input_tokens": scale_tokens(accumulated_input_tokens),
                                                "output_tokens": accumulated_output_tokens,
                                                "cache_creation_input_tokens": 0,
                                                "cache_read_input_tokens": 0
                                            }
                                        });
                                        let sse_data = format!("event: message_delta\ndata: {}\n\n",
                                            serde_json::to_string(&delta_event).unwrap_or_default());
                                        yield Ok(Bytes::from(sse_data));
                                        tracing::info!("📊 Token usage: input={}, output={}", accumulated_input_tokens, accumulated_output_tokens);
                                    }
                                    let event = json!({"type": "message_stop"});
                                    let sse_data = format!("event: message_stop\ndata: {}\n\n",
                                        serde_json::to_string(&event).unwrap_or_default());
                                    yield Ok(Bytes::from(sse_data));
                                    continue;
                                }

                                if let Ok(chunk) = serde_json::from_str::<openai::StreamChunk>(data) {
                                    if message_id.is_none() {
                                        message_id = Some(chunk.id.clone());
                                    }
                                    if current_model.is_none() {
                                        current_model = Some(chunk.model.clone());
                                        tracing::info!("🤖 NIM model: {} (CC alias: {})", chunk.model, original_model_owned);
                                    }

                                    if let Some(usage) = &chunk.usage {
                                        calibration.update(
                                            &nim_model_name,
                                            raw_tiktoken_estimate,
                                            usage.prompt_tokens,
                                        );
                                        accumulated_input_tokens = usage.prompt_tokens;
                                        accumulated_output_tokens = usage.completion_tokens;

                                        // FIX 2: Check if context is nearly full AFTER successful retry.
                                        // When Fixable retry succeeds (reducing max_tokens), the request
                                        // completes but context keeps growing. If scaled tokens exceed the
                                        // configurable threshold (default 80%) of CC's context window, emit
                                        // an error event so CC shows "Use /compact".
                                        let scaled_for_check = scale_tokens(usage.prompt_tokens);
                                        let context_threshold_pct = crate::proxy::get_overflow_threshold_pct();
                                        let context_threshold = cc_context_window * context_threshold_pct / 100;
                                        if scaled_for_check > context_threshold {
                                            tracing::warn!(
                                                "⚠️ Context nearly full ({} scaled tokens = {}% of {}K, threshold={}%) — emitting error to trigger /compact",
                                                scaled_for_check,
                                                scaled_for_check * 100 / cc_context_window,
                                                cc_context_window / 1000,
                                                context_threshold_pct
                                            );
                                            // Close any open content block before emitting error
                                            if current_block_type.is_some() {
                                                let stop_block = json!({"type": "content_block_stop", "index": content_index});
                                                yield Ok(Bytes::from(format!("event: content_block_stop\ndata: {}\n\n", serde_json::to_string(&stop_block).unwrap_or_default())));
                                            }
                                            // Emit error event with invalid_request_error type
                                            // CC treats this as non-retryable and shows "Use /compact" to user
                                            let error_event = json!({
                                                "type": "error",
                                                "error": {
                                                    "type": "invalid_request_error",
                                                    "message": format!(
                                                        "Context window {}% full ({}/{}K, threshold={}%). Use /compact to reduce context.",
                                                        scaled_for_check * 100 / cc_context_window,
                                                        scaled_for_check / 1000,
                                                        cc_context_window / 1000,
                                                        context_threshold_pct
                                                    )
                                                }
                                            });
                                            yield Ok(Bytes::from(format!("event: error\ndata: {}\n\n", serde_json::to_string(&error_event).unwrap_or_default())));
                                            // Clean stream termination
                                            let stop_event = json!({"type": "message_stop"});
                                            yield Ok(Bytes::from(format!("event: message_stop\ndata: {}\n\n", serde_json::to_string(&stop_event).unwrap_or_default())));
                                            break;
                                        }
                                    }

                                    if let Some(choice) = chunk.choices.first() {
                                        if !has_sent_message_start {
                                            let event = anthropic::StreamEvent::MessageStart {
                                                message: anthropic::MessageStartData {
                                                    id: message_id.clone().unwrap_or_default(),
                                                    message_type: "message".to_string(),
                                                    role: "assistant".to_string(),
                                                    model: original_model_owned.clone(),
                                                    usage: anthropic::Usage {
                                                        input_tokens: scale_tokens(
                                                            chunk.usage.as_ref()
                                                                .map(|u| u.prompt_tokens)
                                                                .filter(|&t| t > 0)
                                                                .unwrap_or(estimated_input_tokens)
                                                        ),
                                                        output_tokens: 0,
                                                        cache_creation_input_tokens: Some(0),
                                                        cache_read_input_tokens: Some(0),
                                                        ..Default::default()
                                                    },
                                                },
                                            };
                                            let sse_data = format!("event: message_start\ndata: {}\n\n",
                                                serde_json::to_string(&event).unwrap_or_default());
                                            yield Ok(Bytes::from(sse_data));
                                            has_sent_message_start = true;
                                            tracing::info!("📊 message_start: estimated_input={}", estimated_input_tokens);
                                        }

                                        let reasoning_val = choice.delta.reasoning_content.as_ref()
                                            .or(choice.delta.reasoning.as_ref());
                                        if let Some(reasoning) = reasoning_val {
                                            if !reasoning_poisoned {
                                                let emit_text = if reasoning.contains("<previous_reasoning") {
                                                    let pos = reasoning.find("<previous_reasoning").unwrap_or(0);
                                                    let clean = &reasoning[..pos];
                                                    if clean.trim().is_empty() { None } else { Some(clean.to_string()) }
                                                } else {
                                                    Some(reasoning.to_string())
                                                };

                                                if let Some(text_to_emit) = emit_text {
                                                    if current_block_type.is_none() {
                                                        let event = json!({
                                                            "type": "content_block_start",
                                                            "index": content_index,
                                                            "content_block": {
                                                                "type": "thinking",
                                                                "thinking": "",
                                                                "signature": ""
                                                            }
                                                        });
                                                        let sse_data = format!("event: content_block_start\ndata: {}\n\n",
                                                            serde_json::to_string(&event).unwrap_or_default());
                                                        yield Ok(Bytes::from(sse_data));
                                                        current_block_type = Some("thinking".to_string());
                                                    }

                                                    let event = json!({
                                                        "type": "content_block_delta",
                                                        "index": content_index,
                                                        "delta": {
                                                            "type": "thinking_delta",
                                                            "thinking": text_to_emit
                                                        }
                                                    });
                                                    let sse_data = format!("event: content_block_delta\ndata: {}\n\n",
                                                        serde_json::to_string(&event).unwrap_or_default());
                                                    yield Ok(Bytes::from(sse_data));
                                                }
                                            }
                                        }

                                        if let Some(content) = &choice.delta.content {
                                            if !content.is_empty() {
                                                let sanitized_content = if content.contains("<previous_reasoning") {
                                                    let pos = content.find("<previous_reasoning").unwrap_or(0);
                                                    let clean = &content[..pos];
                                                    if clean.trim().is_empty() { None } else { Some(clean.to_string()) }
                                                } else {
                                                    Some(content.to_string())
                                                };

                                                if let Some(clean_content) = sanitized_content {
                                                    if current_block_type.as_deref() != Some("text") {
                                                        if current_block_type.is_some() {
                                                            let event = json!({"type": "content_block_stop", "index": content_index});
                                                            let sse_data = format!("event: content_block_stop\ndata: {}\n\n",
                                                                serde_json::to_string(&event).unwrap_or_default());
                                                            yield Ok(Bytes::from(sse_data));
                                                            content_index += 1;
                                                        }

                                                        let event = json!({
                                                            "type": "content_block_start",
                                                            "index": content_index,
                                                            "content_block": {
                                                                "type": "text",
                                                                "text": ""
                                                            }
                                                        });
                                                        let sse_data = format!("event: content_block_start\ndata: {}\n\n",
                                                            serde_json::to_string(&event).unwrap_or_default());
                                                        yield Ok(Bytes::from(sse_data));
                                                        current_block_type = Some("text".to_string());
                                                    }

                                                    let event = json!({
                                                        "type": "content_block_delta",
                                                        "index": content_index,
                                                        "delta": {
                                                            "type": "text_delta",
                                                            "text": clean_content
                                                        }
                                                    });
                                                    let sse_data = format!("event: content_block_delta\ndata: {}\n\n",
                                                        serde_json::to_string(&event).unwrap_or_default());
                                                    yield Ok(Bytes::from(sse_data));
                                                }
                                            }
                                        }

                                        if let Some(tool_calls) = &choice.delta.tool_calls {
                                            for tool_call in tool_calls {
                                                if let Some(id) = &tool_call.id {
                                                    if current_block_type.is_some() && !is_intercepting_fetch {
                                                        let event = json!({"type": "content_block_stop", "index": content_index});
                                                        let sse_data = format!("event: content_block_stop\ndata: {}\n\n",
                                                            serde_json::to_string(&event).unwrap_or_default());
                                                        yield Ok(Bytes::from(sse_data));
                                                        content_index += 1;
                                                    }
                                                    tool_call_id = Some(id.clone());
                                                    tool_call_args.clear();
                                                    tool_calls_emitted = true;
                                                }

                                                if let Some(function) = &tool_call.function {
                                                    if let Some(name) = &function.name {
                                                        _tool_call_name = Some(name.clone());

                                                        if web_fetch::is_web_fetch_tool(name) {
                                                            is_intercepting_fetch = true;
                                                            _fetch_tool_name = Some(name.clone());
                                                            _fetch_tool_id_buf = tool_call_id.clone();
                                                            fetch_args_buffer.clear();
                                                            suppressed_block_start = true;
                                                            tracing::info!("[WebFetch/Stream] Intercepting tool_use: {}", name);
                                                        } else {
                                                            let event = json!({
                                                                "type": "content_block_start",
                                                                "index": content_index,
                                                                "content_block": {
                                                                    "type": "tool_use",
                                                                    "id": tool_call_id.clone().unwrap_or_default(),
                                                                    "name": name
                                                                }
                                                            });
                                                            let sse_data = format!("event: content_block_start\ndata: {}\n\n",
                                                                serde_json::to_string(&event).unwrap_or_default());
                                                            yield Ok(Bytes::from(sse_data));
                                                            current_block_type = Some("tool_use".to_string());
                                                        }
                                                    }

                                                    if let Some(args) = &function.arguments {
                                                        tool_call_args.push_str(args);

                                                        if is_intercepting_fetch {
                                                            fetch_args_buffer.push_str(args);
                                                        } else {
                                                            let event = json!({
                                                                "type": "content_block_delta",
                                                                "index": content_index,
                                                                "delta": {
                                                                    "type": "input_json_delta",
                                                                    "partial_json": args
                                                                }
                                                            });
                                                            let sse_data = format!("event: content_block_delta\ndata: {}\n\n",
                                                                serde_json::to_string(&event).unwrap_or_default());
                                                            yield Ok(Bytes::from(sse_data));
                                                        }
                                                    }
                                                }
                                            }
                                        }

                                        if let Some(finish_reason) = &choice.finish_reason {
                                            if is_intercepting_fetch {
                                                let fetch_url = web_fetch::extract_url_from_raw(&fetch_args_buffer)
                                                    .unwrap_or_default();

                                                let fetch_result = if fetch_url.is_empty() || !fetch_url.starts_with("http") {
                                                    "Error: Could not extract valid URL".to_string()
                                                } else {
                                                    tracing::info!("[WebFetch/Stream] Executing fetch for: {}", fetch_url);
                                                    let fetch_client = reqwest::Client::builder()
                                                        .timeout(std::time::Duration::from_secs(15))
                                                        .build()
                                                        .unwrap_or_else(|_| reqwest::Client::new());

                                                    match fetch_client
                                                        .get(&fetch_url)
                                                        .header("User-Agent", "Mozilla/5.0")
                                                        .send()
                                                        .await
                                                    {
                                                        Ok(resp) => {
                                                            match resp.text().await {
                                                                Ok(body) => {
                                                                    let text = web_fetch::strip_html_tags(&body);
                                                                    if text.len() > 200_000 {
                                                                        format!("{}\n\n[Content truncated]", &text[..200_000])
                                                                    } else {
                                                                        text
                                                                    }
                                                                }
                                                                Err(e) => format!("Error reading response: {}", e),
                                                            }
                                                        }
                                                        Err(e) => format!("Error fetching {}: {}", fetch_url, e),
                                                    }
                                                };

                                                tracing::info!("[WebFetch/Stream] Fetched {} chars", fetch_result.len());

                                                if suppressed_block_start {
                                                    let event = json!({
                                                        "type": "content_block_start",
                                                        "index": content_index,
                                                        "content_block": {
                                                            "type": "text",
                                                            "text": ""
                                                        }
                                                    });
                                                    let sse_data = format!("event: content_block_start\ndata: {}\n\n",
                                                        serde_json::to_string(&event).unwrap_or_default());
                                                    yield Ok(Bytes::from(sse_data));
                                                }

                                                let event = json!({
                                                    "type": "content_block_delta",
                                                    "index": content_index,
                                                    "delta": {
                                                        "type": "text_delta",
                                                        "text": format!("\n\n[Fetched content from {}]\n\n{}", fetch_url, fetch_result)
                                                    }
                                                });
                                                let sse_data = format!("event: content_block_delta\ndata: {}\n\n",
                                                    serde_json::to_string(&event).unwrap_or_default());
                                                yield Ok(Bytes::from(sse_data));

                                                current_block_type = Some("text".to_string());
                                                is_intercepting_fetch = false;
                                                suppressed_block_start = false;
                                            }

                                            if current_block_type.is_some() {
                                                let event = json!({"type": "content_block_stop", "index": content_index});
                                                let sse_data = format!("event: content_block_stop\ndata: {}\n\n",
                                                    serde_json::to_string(&event).unwrap_or_default());
                                                yield Ok(Bytes::from(sse_data));
                                            }

                                            let stop_reason = transform::map_stop_reason(Some(finish_reason), tool_calls_emitted);
                                            saved_stop_reason = stop_reason;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Stream error: {}", e);
                    let error_event = json!({
                        "type": "error",
                        "error": {
                            "type": "api_error",
                            "message": format!("Stream error: {}", e)
                        }
                    });
                    let sse_data = format!("event: error\ndata: {}\n\n",
                        serde_json::to_string(&error_event).unwrap_or_default());
                    yield Ok(Bytes::from(sse_data));
                    break;
                }
            }
        }
    }
}
