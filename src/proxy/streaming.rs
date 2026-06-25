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
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::error::ProxyResult;
use crate::models::{anthropic, openai};
use crate::proxy::concurrency::{acquire_model_permit, ModelSemaphores};
use crate::proxy::retry::{chunk_timeout_secs, resilient_send_raw, MAX_SSE_BUFFER};
use crate::proxy::token_scaling::scale_token_usage;
use crate::tokenizer;
use crate::transform;
use crate::web_fetch;

/// Interval for anthropic.keep-alive SSE events (prevents CC timeout on slow upstreams)
pub(crate) const KEEPALIVE_INTERVAL_SECS: u64 = 30;

/// Build the `signature_delta` SSE event for a completed NEXUS thinking block, per the
/// configured signature mode. Returns `None` when no signature should be emitted (mode
/// `omit`/`durable`, or empty thinking). Issue #90-B (ARB L3/L4): completes the thinking
/// sub-protocol that NEXUS previously skipped (it emitted `signature:""` and never a
/// `signature_delta`).
fn signature_delta_sse(thinking: &str, index: i32) -> Option<Bytes> {
    let sig = crate::reasoning::signature::reasoning_signature(thinking)?;
    let ev = json!({
        "type": "content_block_delta",
        "index": index,
        "delta": { "type": "signature_delta", "signature": sig }
    });
    Some(Bytes::from(format!(
        "event: content_block_delta\ndata: {}\n\n",
        serde_json::to_string(&ev).unwrap_or_default()
    )))
}

/// Issue #106: should the silent-turn-death guard fire? True when a turn was started
/// (message_start sent) but no usable content — neither text nor a tool_use — ever reached
/// the client, so the stream would otherwise close content-empty and the CC turn would die
/// silently (the reasoning-only / finish_reason=length case under forced thinking).
fn needs_empty_content_guard(
    message_started: bool,
    content_emitted: bool,
    tool_calls_emitted: bool,
) -> bool {
    message_started && !content_emitted && !tool_calls_emitted
}

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
    shutdown_token: CancellationToken,
    client_headers: crate::proxy::headers::ClientHeaders,
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

    let response = resilient_send_raw(
        &client,
        &config,
        &mut mutable_req,
        upstream_name,
        _circuit_breaker,
        &client_headers,
    )
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
        shutdown_token,
        config.clone(),
        client.clone(),
    );

    let mut headers = HeaderMap::new();
    headers.insert("Content-Type", HeaderValue::from_static("text/event-stream"));
    headers.insert("Cache-Control", HeaderValue::from_static("no-cache"));
    headers.insert("Connection", HeaderValue::from_static("keep-alive"));

    Ok((headers, Body::from_stream(sse_stream)).into_response())
}

#[allow(clippy::too_many_arguments)]
#[allow(unused_assignments)]
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
    shutdown_token: CancellationToken,
    // Issue #64/#65 (Solution A): shared HTTP client + config so the streaming WebFetch
    // interceptor can delegate to web_fetch::execute_fetch() (validated, UTF-8-safe).
    config: Arc<Config>,
    client: Client,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
    async_stream::stream! {
        tracing::debug!(
            "[CALIB] Auto-compact scaling: cc_context_window={}K, model_context={}K",
            cc_context_window / 1000,
            context_limit / 1000
        );
        let cc_ctx = cc_context_window;
        let upstream_ctx = context_limit;

        let mut permit_opt: Option<tokio::sync::OwnedSemaphorePermit> = Some(_permit);

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
        // Issue #106: did we forward any *usable* content (text) to CC? A reasoning-only
        // completion (forced thinking on a large context spends the whole budget on
        // reasoning_content -> finish_reason=length, 0 content) would otherwise produce a
        // content-empty stream that kills the CC turn silently.
        let mut content_emitted = false;
        let mut is_intercepting_fetch = false;
        let mut _fetch_tool_name: Option<String> = None;
        let mut _fetch_tool_id_buf: Option<String> = None;
        let mut fetch_args_buffer = String::new();
        let mut suppressed_block_start = false;
        let mut accumulated_input_tokens: u32 = estimated_input_tokens;
        let mut accumulated_output_tokens: u32 = 0;
        let mut saved_stop_reason: Option<String> = None;
        let mut reasoning_poisoned = false;
        // Issue #90-B (ARB L4): accumulate the emitted thinking so a `signature_delta`
        // (provenance token) can be emitted before the thinking block closes.
        let mut accumulated_thinking = String::new();

        tokio::pin!(stream);

    let mut last_keepalive = std::time::Instant::now();
    // Use min of KEEPALIVE_INTERVAL and CHUNK_TIMEOUT for the select timeout
    // This allows us to emit keep-alive even when CHUNK_TIMEOUT is 120s
    let chunk_timeout = chunk_timeout_secs();
    let keepalive_check_interval = Duration::from_secs(KEEPALIVE_INTERVAL_SECS.min(chunk_timeout));
    let mut consecutive_keepalives: u32 = 0;

    loop {
        let chunk = tokio::select! {
            chunk = tokio::time::timeout(keepalive_check_interval, stream.next()) => {
                match chunk {
                    Ok(Some(c)) => {
                        consecutive_keepalives = 0;
                        c
                    }
                    Ok(None) => break,
                    Err(_) => {
                        // Timeout — check if we need keep-alive or if it's a real timeout
                        if last_keepalive.elapsed().as_secs() >= KEEPALIVE_INTERVAL_SECS {
                            // Emit keep-alive event to prevent CC timeout
                            tracing::trace!("📡 Sending anthropic.keep-alive SSE event");
                            yield Ok(Bytes::from("event: anthropic.keep-alive\ndata: {}\n\n"));
                            last_keepalive = std::time::Instant::now();
                            consecutive_keepalives += 1;
                            // If too many consecutive keepalives, the upstream is truly dead
                            let max_consecutive = (chunk_timeout / KEEPALIVE_INTERVAL_SECS).max(1);
                            if consecutive_keepalives >= max_consecutive as u32 {
                                tracing::error!("⏰ Stream chunk timeout after {}s — emitting error event", chunk_timeout);
                                // CR7: Emit error event unconditionally — client needs it even before message_start
                                let error_event = json!({
                                    "type": "error",
                                    "error": {
                                        "type": "api_error",
                                        "message": format!(
                                            "Stream timeout: upstream stopped responding after {}s. The response may be incomplete.",
                                            chunk_timeout
                                        )
                                    }
                                });
                                yield Ok(Bytes::from(format!("event: error\ndata: {}\n\n", serde_json::to_string(&error_event).unwrap_or_default())));
                                if has_sent_message_start {
                                    // Close any open content blocks
                                    if current_block_type.is_some() {
                                        let stop_block = json!({"type": "content_block_stop", "index": content_index});
                                        yield Ok(Bytes::from(format!("event: content_block_stop\ndata: {}\n\n", serde_json::to_string(&stop_block).unwrap_or_default())));
                                    }
                                    // Emit message_stop to cleanly terminate the stream
                                    let stop_event = json!({"type": "message_stop"});
                                    yield Ok(Bytes::from(format!("event: message_stop\ndata: {}\n\n", serde_json::to_string(&stop_event).unwrap_or_default())));
                                }
                                break;
                            }
                            continue; // ← Keep listening, don't break
                        }
                        // Short timeout but not enough for keep-alive — continue listening
                        continue;
                    }
                }
            }
            _ = shutdown_token.cancelled() => {
                // Shutdown signal — emit graceful error event before closing
                tracing::info!("🛑 SSE stream: shutdown signal received — emitting graceful error");
                // CR7: Emit error event unconditionally — client needs it even before message_start
                let error_event = json!({
                    "type": "error",
                    "error": {
                        "type": "api_error",
                        "message": "Server is shutting down for restart. Please retry — your request will be processed by the new instance."
                    }
                });
                yield Ok(Bytes::from(format!("event: error\ndata: {}\n\n", serde_json::to_string(&error_event).unwrap_or_default())));
                if has_sent_message_start {
                    // Close any open content block
                    if current_block_type.is_some() {
                        let stop_block = json!({"type": "content_block_stop", "index": content_index});
                        yield Ok(Bytes::from(format!("event: content_block_stop\ndata: {}\n\n", serde_json::to_string(&stop_block).unwrap_or_default())));
                    }
                    // Emit message_stop for clean stream termination
                    let stop_event = json!({"type": "message_stop"});
                    yield Ok(Bytes::from(format!("event: message_stop\ndata: {}\n\n", serde_json::to_string(&stop_event).unwrap_or_default())));
                }
                break;
            }
        };

        // Reset keepalive timer when we receive a real chunk
        last_keepalive = std::time::Instant::now();
        consecutive_keepalives = 0;

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
                                    // Issue #64/#65/A1: flush a pending WebFetch interception. With NIM the
                                    // finish_reason branch is not reliably reached after a web_fetch tool_call,
                                    // so without this the interception is abandoned and the model gets nothing
                                    // (A1). Execute it here via execute_fetch (validated + UTF-8-safe) and emit
                                    // the content as a self-contained text block. The is_intercepting_fetch
                                    // guard makes this idempotent with the finish_reason path.
                                    if is_intercepting_fetch {
                                        let fetch_url = web_fetch::extract_url_from_raw(&fetch_args_buffer).unwrap_or_default();
                                        let fetch_result = if fetch_url.is_empty() || !fetch_url.starts_with("http") {
                                            "Error: Could not extract valid URL".to_string()
                                        } else {
                                            tracing::info!("[WebFetch/Stream] Executing fetch for: {} (flushed at [DONE])", fetch_url);
                                            web_fetch::execute_fetch(&client, &fetch_url, &config)
                                                .await
                                                .unwrap_or_else(|e| format!("Error fetching {}: {}", fetch_url, e))
                                        };
                                        tracing::info!("[WebFetch/Stream] Fetched {} chars (flushed at [DONE])", fetch_result.len());
                                        if current_block_type.is_some() {
                                            let ev = json!({"type": "content_block_stop", "index": content_index});
                                            yield Ok(Bytes::from(format!("event: content_block_stop\ndata: {}\n\n", serde_json::to_string(&ev).unwrap_or_default())));
                                            content_index += 1;
                                        }
                                        let ev = json!({"type": "content_block_start", "index": content_index, "content_block": {"type": "text", "text": ""}});
                                        yield Ok(Bytes::from(format!("event: content_block_start\ndata: {}\n\n", serde_json::to_string(&ev).unwrap_or_default())));
                                        let ev = json!({"type": "content_block_delta", "index": content_index, "delta": {"type": "text_delta", "text": format!("[Fetched content from {}]\n\n{}", fetch_url, fetch_result)}});
                                        yield Ok(Bytes::from(format!("event: content_block_delta\ndata: {}\n\n", serde_json::to_string(&ev).unwrap_or_default())));
                                        let ev = json!({"type": "content_block_stop", "index": content_index});
                                        yield Ok(Bytes::from(format!("event: content_block_stop\ndata: {}\n\n", serde_json::to_string(&ev).unwrap_or_default())));
                                        content_index += 1;
                                        current_block_type = None;
                                        content_emitted = true; // Issue #106: fetch text is content
                                        is_intercepting_fetch = false;
                                        suppressed_block_start = false;
                                        // CodeRabbit: when the finish_reason branch never ran (the common NIM
                                        // web_fetch case), saved_stop_reason is None and the terminal
                                        // message_delta (stop_reason + scaled usage) below would be skipped.
                                        // Set it so the stream ends with a proper message_delta, matching the
                                        // non-streaming contract.
                                        if saved_stop_reason.is_none() {
                                            saved_stop_reason = Some("end_turn".to_string());
                                        }
                                    }
                                    // Issue #106: silent turn death guard. If a turn was started (message_start
                                    // sent) but ZERO usable content reached CC — only reasoning_content, which
                                    // happens when forced thinking on a large context spends the whole output
                                    // budget reasoning (finish_reason=length) — inject a synthetic text block so
                                    // CC receives a visible, explained turn instead of a content-empty stream
                                    // that dies silently. tool_calls_emitted covers valid tool_use turns.
                                    if needs_empty_content_guard(has_sent_message_start, content_emitted, tool_calls_emitted) {
                                        if current_block_type.is_some() {
                                            let ev = json!({"type": "content_block_stop", "index": content_index});
                                            yield Ok(Bytes::from(format!("event: content_block_stop\ndata: {}\n\n", serde_json::to_string(&ev).unwrap_or_default())));
                                            content_index += 1;
                                            current_block_type = None;
                                        }
                                        let notice = "[NEXUS] The model returned no answer: it spent its entire output budget on internal reasoning for this large context and produced no content (finish_reason=length). Try /compact to shrink the context, or retry — the request may be too large for thinking mode.";
                                        let ev = json!({"type": "content_block_start", "index": content_index, "content_block": {"type": "text", "text": ""}});
                                        yield Ok(Bytes::from(format!("event: content_block_start\ndata: {}\n\n", serde_json::to_string(&ev).unwrap_or_default())));
                                        let ev = json!({"type": "content_block_delta", "index": content_index, "delta": {"type": "text_delta", "text": notice}});
                                        yield Ok(Bytes::from(format!("event: content_block_delta\ndata: {}\n\n", serde_json::to_string(&ev).unwrap_or_default())));
                                        let ev = json!({"type": "content_block_stop", "index": content_index});
                                        yield Ok(Bytes::from(format!("event: content_block_stop\ndata: {}\n\n", serde_json::to_string(&ev).unwrap_or_default())));
                                        content_index += 1;
                                        content_emitted = true;
                                        // Close the turn cleanly so CC shows the notice instead of treating a
                                        // bare finish_reason=length (max_tokens) as a truncation to continue.
                                        saved_stop_reason = Some("end_turn".to_string());
                                        tracing::warn!("[STREAM] Issue #106 guard: upstream emitted 0 content tokens (reasoning-only) - injected fallback to prevent silent turn death");
                                    }
                                    if let Some(ref stop) = saved_stop_reason {
                                        let scaled_delta = scale_token_usage(accumulated_input_tokens, accumulated_output_tokens, upstream_ctx, cc_ctx, "streaming-delta");
                                        let delta_event = json!({
                                            "type": "message_delta",
                                            "delta": {
                                                "stop_reason": stop,
                                                "stop_sequence": serde_json::Value::Null,
                                            },
                                            "usage": {
                                                "input_tokens": scaled_delta.input,
                                                "output_tokens": scaled_delta.output,
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

                                        // S7: Release semaphore permit early once stream has output tokens
                                        if accumulated_output_tokens > 0 {
                                            if let Some(p) = permit_opt.take() {
                                                drop(p);
                                                tracing::debug!("🔓 Semaphore permit released — stream has output tokens");
                                            }
                                        }

                                        // FIX 2: Check if context is nearly full AFTER successful retry.
                                        // When Fixable retry succeeds (reducing max_tokens), the request
                                        // completes but context keeps growing. If scaled tokens exceed the
                                        // configurable threshold (default 90%) of CC's context window, emit
                                        // an error event so CC shows "Use /compact".
                                        // Only .input needed; output=0 because input/output scaling are independent
                                        let scaled_for_check = scale_token_usage(usage.prompt_tokens, 0, upstream_ctx, cc_ctx, "streaming-overflow").input;
                                        let context_threshold_pct = crate::proxy::get_overflow_threshold_pct();
                                        let context_threshold = cc_context_window * context_threshold_pct / 100;
                                        if scaled_for_check > context_threshold {
                                            tracing::warn!(
                                                "[WARN] Context nearly full ({} scaled tokens = {}% of {}K, threshold={}%) — emitting error to trigger /compact",
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
                                                        input_tokens: {
                                                            let start_input = chunk.usage.as_ref()
                                                                .map(|u| u.prompt_tokens)
                                                                .filter(|&t| t > 0)
                                                                .unwrap_or(estimated_input_tokens);
                                                            // Only .input needed; output=0 because message_start has no output tokens yet
                                                            let scaled_start = scale_token_usage(start_input, 0, upstream_ctx, cc_ctx, "streaming-start");
                                                            scaled_start.input
                                                        },
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

                            // S7: Release semaphore permit early once stream is warm
                            // This allows new requests to start while this stream continues flowing
                            if accumulated_output_tokens > 0 {
                                if let Some(p) = permit_opt.take() {
                                    drop(p);
                                    tracing::debug!("🔓 Semaphore permit released — stream is flowing");
                                }
                            }
                                        }

                                        let reasoning_val = choice.delta.reasoning_content.as_ref()
                                            .or(choice.delta.reasoning.as_ref());
                                        if let Some(reasoning) = reasoning_val {
                                            if reasoning.contains("<previous_reasoning") {
                                                reasoning_poisoned = true;
                                            }
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

                                                    // Issue #90-B: accumulate for the closing signature_delta.
                                                    accumulated_thinking.push_str(&text_to_emit);
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
                                            } else {
                                                // FIX C7 (Issue #60): Reasoning is poisoned — suppress further reasoning output
                                                tracing::trace!("reasoning_poisoned=true: suppressing reasoning output");
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

                                                // Check if we're dealing with <previous_reasoning> content
                                                if content.contains("<previous_reasoning") {
                                                    reasoning_poisoned = true;
                                                    tracing::debug!("reasoning_poisoned = true -- <previous_reasoning> detected in stream");
                                                }

                                                // FIX C4 (Issue #80): If current block is tool_use and sanitized_content is None,
                                                // the content was discarded by sanitization (<previous_reasoning>), but the model
                                                // already transitioned to generating text — close the tool_use block first.
                                                if current_block_type.as_deref() == Some("tool_use") && sanitized_content.is_none() {
                                                    let event = json!({"type": "content_block_stop", "index": content_index});
                                                    let sse_data = format!("event: content_block_stop\ndata: {}\n\n",
                                                        serde_json::to_string(&event).unwrap_or_default());
                                                    yield Ok(Bytes::from(sse_data));
                                                    content_index += 1;
                                                    current_block_type = None;
                                                }

                                                if let Some(clean_content) = sanitized_content {
                                                    if current_block_type.as_deref() != Some("text") {
                                                        if current_block_type.is_some() {
                                                            // Issue #90-B: close the thinking sub-protocol with a signature_delta.
                                                            if current_block_type.as_deref() == Some("thinking") {
                                                                if let Some(b) = signature_delta_sse(&accumulated_thinking, content_index) {
                                                                    yield Ok(b);
                                                                }
                                                                accumulated_thinking.clear();
                                                            }
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
                                                    content_emitted = true; // Issue #106
                                                }
                                            }
                                        }

                                        if let Some(tool_calls) = &choice.delta.tool_calls {
                                            for tool_call in tool_calls {
                                                if let Some(id) = &tool_call.id {
                                                    if current_block_type.is_some() && !is_intercepting_fetch {
                                                        // Issue #90-B: close the thinking sub-protocol with a signature_delta.
                                                        if current_block_type.as_deref() == Some("thinking") {
                                                            if let Some(b) = signature_delta_sse(&accumulated_thinking, content_index) {
                                                                yield Ok(b);
                                                            }
                                                            accumulated_thinking.clear();
                                                        }
                                                        let event = json!({"type": "content_block_stop", "index": content_index});
                                                        let sse_data = format!("event: content_block_stop\ndata: {}\n\n",
                                                            serde_json::to_string(&event).unwrap_or_default());
                                                        yield Ok(Bytes::from(sse_data));
                                                        content_index += 1;
                                                        // A5 fix (Issue #64/#65 empirical): reset block state after
                                                        // closing. Without this, current_block_type stays stale (e.g.
                                                        // "thinking") and the subsequent WebFetch/text emit double-
                                                        // closes this already-closed block, emitting an orphan
                                                        // content_block_stop that CC rejects as "Content block not found".
                                                        current_block_type = None;
                                                    }
                                                    // Issue #90: sanitize at the source so both the
                                                    // emitted tool_use.id and the web_fetch buffer are valid.
                                                    tool_call_id =
                                                        Some(crate::tool_id::sanitize_tool_id(id, content_index as usize));
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
                                                                    "id": tool_call_id.clone().unwrap_or_else(|| format!("toolu_{content_index}")),
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
                                                    } else if !is_intercepting_fetch && current_block_type.as_deref() == Some("tool_use") {
                                                        // FIX C10 (Issue #80): If arguments is None but tool_use is open and we
                                                        // already have previous arguments, emit empty input_json_delta to keep
                                                        // the block valid. Without this, the tool_use block stays open with no deltas.
                                                        let event = json!({
                                                            "type": "content_block_delta",
                                                            "index": content_index,
                                                            "delta": { "type": "input_json_delta", "partial_json": "" }
                                                        });
                                                        let sse_data = format!("event: content_block_delta\ndata: {}\n\n",
                                                            serde_json::to_string(&event).unwrap_or_default());
                                                        yield Ok(Bytes::from(sse_data));
                                                    }
                                                }
                                            }
                                        }

                                        if let Some(finish_reason) = &choice.finish_reason {
                                            if is_intercepting_fetch {
                                                let fetch_url = web_fetch::extract_url_from_raw(&fetch_args_buffer)
                                                    .unwrap_or_default();

                                                // Issue #64 + #65 (Solution A): delegate to the shared, validated,
                                                // UTF-8-safe execute_fetch() instead of an ad-hoc client. This
                                                // inherits is_url_safe() + DNS-aware SSRF guard (no more #64 bypass)
                                                // and safe_truncate() (no more #65 byte-slice panic), and honors
                                                // config timeout / content-type handling — mirroring non_streaming.
                                                let fetch_result = if fetch_url.is_empty() || !fetch_url.starts_with("http") {
                                                    "Error: Could not extract valid URL".to_string()
                                                } else {
                                                    tracing::info!("[WebFetch/Stream] Executing fetch for: {}", fetch_url);
                                                    web_fetch::execute_fetch(&client, &fetch_url, &config)
                                                        .await
                                                        .unwrap_or_else(|e| format!("Error fetching {}: {}", fetch_url, e))
                                                };

                                                tracing::info!("[WebFetch/Stream] Fetched {} chars", fetch_result.len());

                                                if suppressed_block_start {
                                                    // FIX C11 (Issue #80): Close any prior tool_use block that might be
                                                    // open (from another tool_call in the same chunk) before emitting text.
                                                    if current_block_type.is_some() {
                                                        // Issue #90-B: close the thinking sub-protocol with a signature_delta.
                                                        if current_block_type.as_deref() == Some("thinking") {
                                                            if let Some(b) = signature_delta_sse(&accumulated_thinking, content_index) {
                                                                yield Ok(b);
                                                            }
                                                            accumulated_thinking.clear();
                                                        }
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
                                                content_emitted = true; // Issue #106: fetch text is content
                                                is_intercepting_fetch = false;
                                                suppressed_block_start = false;
                                            }

                                            if current_block_type.is_some() {
                                                // Issue #90-B: close the thinking sub-protocol with a signature_delta.
                                                if current_block_type.as_deref() == Some("thinking") {
                                                    if let Some(b) = signature_delta_sse(&accumulated_thinking, content_index) {
                                                        yield Ok(b);
                                                    }
                                                    accumulated_thinking.clear();
                                                }
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
                    // CR4-fix: Diagnose specific error type for forensics
                    let err_detail = format!(
                        "timeout={}, connect={}, decode={}, body={}, status={:?}",
                        e.is_timeout(), e.is_connect(), e.is_decode(), e.is_body(), e.status()
                    );
                    tracing::error!("Stream error: {} [{}]", e, err_detail);
                    let error_event = json!({
                        "type": "error",
                        "error": {
                            "type": "api_error",
                            "message": format!(
                                "Stream error: {}. This is a transient upstream error — retry should succeed.",
                                e
                            )
                        }
                    });
                    let sse_data = format!("event: error\ndata: {}\n\n",
                        serde_json::to_string(&error_event).unwrap_or_default());
                    yield Ok(Bytes::from(sse_data));

                    // CR4-fix: Close stream cleanly so CC doesn't hang
                    if has_sent_message_start {
                        if current_block_type.is_some() {
                            let stop_block = json!({"type": "content_block_stop", "index": content_index});
                            yield Ok(Bytes::from(format!("event: content_block_stop\ndata: {}\n\n",
                                serde_json::to_string(&stop_block).unwrap_or_default())));
                        }
                        let stop_event = json!({"type": "message_stop"});
                        yield Ok(Bytes::from(format!("event: message_stop\ndata: {}\n\n",
                            serde_json::to_string(&stop_event).unwrap_or_default())));
                    }
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod stream_guard_tests {
    use super::needs_empty_content_guard;

    #[test]
    fn fires_on_reasoning_only_turn() {
        // Turn started, no text, no tool_use -> the #106 silent-death condition.
        assert!(needs_empty_content_guard(true, false, false));
    }

    #[test]
    fn does_not_fire_when_text_emitted() {
        assert!(!needs_empty_content_guard(true, true, false));
    }

    #[test]
    fn does_not_fire_for_tool_use_turn() {
        assert!(!needs_empty_content_guard(true, false, true));
    }

    #[test]
    fn does_not_fire_before_message_start() {
        assert!(!needs_empty_content_guard(false, false, false));
    }
}
