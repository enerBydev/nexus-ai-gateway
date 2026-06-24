use std::time::Duration;

use reqwest::Client;

use crate::config::{Config, UpstreamType};
use crate::error::{ProxyError, ProxyResult};
use crate::models::openai;
use crate::proxy::classify::{
    classify_error, delay_with_jitter, error_regexes, extract_safe_max_tokens_from_error,
    ErrorClass, MAX_RETRIES, MIN_CLAMP_TOKENS,
};
use crate::proxy::error_types::parse_upstream_error;
use crate::proxy::overflow_tracker::OverflowLoopTracker;

// v0.11.0: Stream stability constants (CR-01, CR-02)
// CR3-fix: Configurable via CHUNK_TIMEOUT_SECS env var (default 120s)
pub(crate) fn chunk_timeout_secs() -> u64 {
    std::env::var("CHUNK_TIMEOUT_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(120)
}
pub(crate) const MAX_SSE_BUFFER: usize = 10 * 1024 * 1024; // 10MB safety limit for SSE buffer

/// Compute the clamped `max_tokens` for a retry on a clampable (`Fixable`) upstream error.
///
/// Issue #62: single source of truth for the retry-clamp decision that previously lived
/// duplicated in both the non-streaming (`resilient_send`) and streaming
/// (`resilient_send_raw`) loops — keeping one copy prevents the two from drifting (the
/// asymmetry this issue tracked, already unified for behavior by Issue #34 M9).
///
/// On an `input_tokens overflow` the safe limit is extracted from the NIM message and
/// reduced by a growing safety margin (NIM re-tokenization adds ~257 tokens per retry from
/// chat-template expansion); otherwise it falls back to halving. `current_max` is the
/// caller's current `max_tokens`; `stream_label` is "" or "[stream] " for log parity.
fn clamp_max_tokens_for_retry(
    status: reqwest::StatusCode,
    reason: &str,
    message: &str,
    current_max: u32,
    attempt: u32,
    stream_label: &str,
) -> u32 {
    if reason.contains("input_tokens overflow") {
        if let Some(safe) = extract_safe_max_tokens_from_error(message) {
            let margin = 2048 + (attempt * 1024); // Growing margin per retry
            let safe_with_margin = safe.saturating_sub(margin).max(1024);
            tracing::warn!(
                "🔧 {}input_tokens overflow (attempt {}/{}): NIM safe={}, margin={}, clamping max_tokens -> {}",
                stream_label, attempt, MAX_RETRIES, safe, margin, safe_with_margin
            );
            safe_with_margin
        } else {
            (current_max / 2).max(MIN_CLAMP_TOKENS)
        }
    } else {
        let halved = (current_max / 2).max(MIN_CLAMP_TOKENS);
        tracing::warn!(
            "🔧 {}{} [{}] (attempt {}/{}): clamping max_tokens {} -> {}",
            stream_label,
            status.as_u16(),
            reason,
            attempt,
            MAX_RETRIES,
            current_max,
            halved
        );
        halved
    }
}

/// Resilient send for non-streaming: returns parsed OpenAI response.
/// Auto-retries on 429 (rate limit) with exponential backoff.
/// Auto-clamps max_tokens on 400 (too large) and retries.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn resilient_send(
    client: &Client,
    config: &Config,
    openai_req: &mut openai::OpenAIRequest,
    upstream_name: &str,
    circuit_breaker: &crate::proxy::concurrency::CircuitBreaker,
    client_headers: &crate::proxy::headers::ClientHeaders,
) -> ProxyResult<openai::OpenAIResponse> {
    let mut attempt: u32 = 0;

    loop {
        attempt += 1;

        // S8: Fail-fast when server is draining — skip retries only, not first attempt
        if attempt > 1 && crate::IS_DRAINING.load(std::sync::atomic::Ordering::Relaxed) {
            tracing::warn!("Server is draining — skipping retry (attempt {})", attempt);
            return Err(crate::error::ProxyError::Upstream(
                "Server is shutting down — request not retried".to_string(),
            ));
        }

        // Circuit breaker check
        let (allowed, generation) = circuit_breaker.is_allowed().await;
        if !allowed {
            tracing::warn!("Circuit breaker OPEN — rejecting request (upstream unhealthy)");
            return Err(ProxyError::Upstream(
                "Service unavailable: circuit breaker open".to_string(),
            ));
        }

        let mut req_builder = client
            .post(config.get_upstream_url(upstream_name))
            .json(&*openai_req)
            .timeout(Duration::from_secs(300)); // P0: was 900s — must be < CC's API_TIMEOUT_MS (600s)

        if let Some(api_key) = &config.get_upstream_key(upstream_name) {
            req_builder = req_builder.header("Authorization", format!("Bearer {}", api_key));
        }

        // Issue #35: Forward anthropic-beta + anthropic-version to Anthropic upstreams
        if config.get_upstream_type(upstream_name) == UpstreamType::Anthropic {
            // Bug B: Forward merged betas (client + proxy minimum, deduplicated)
            // Bug C: Auto-resolved — no more hardcoded outdated beta value
            // User Decision Q1 (Option C): merge client + proxy betas
            // User Decision Q2 (Option B): always inject minimum set
            let beta = client_headers.resolve_anthropic_beta();
            let version = client_headers.resolve_anthropic_version();
            tracing::debug!(
                "📤 Forwarding to Anthropic upstream '{}': beta={}, version={}",
                upstream_name,
                beta,
                version
            );
            req_builder = req_builder.header("anthropic-beta", &beta);
            // Bug D: Forward anthropic-version with fallback
            // User Decision Q5 (Option A): forward with default
            req_builder = req_builder.header("anthropic-version", version);
        }

        let response = req_builder.send().await?;
        let status = response.status();

        if status.is_success() {
            circuit_breaker.record_success(generation).await;
            OverflowLoopTracker::reset_tracker(&openai_req.model);
            let resp: openai::OpenAIResponse = response.json().await?;
            if attempt > 1 {
                tracing::info!("🔄 Request succeeded on attempt #{}", attempt);
            }
            return Ok(resp);
        }

        let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());

        // === Smart Retry: 3-Layer Error Classification ===
        let upstream_err = parse_upstream_error(status.as_u16(), &error_text);
        tracing::debug!(
            "[SCAN] Parsed error: status={}, type={:?}, param={:?}, msg={}",
            upstream_err.status,
            upstream_err.error_type,
            upstream_err.param,
            crate::str_utils::safe_truncate(&upstream_err.message, 100)
        );

        let class = classify_error(&upstream_err);
        tracing::debug!("🧠 Classified: {:?} (status={})", class, status.as_u16());

        match class {
            ErrorClass::Retryable { base_delay_ms, max_retries, reason } => {
                if attempt >= max_retries {
                    // Only record ONE CB failure when ALL retries are exhausted.
                    // Internal retries are self-healing — they must not individually
                    // trip the breaker (3 retries of 1 request ≠ 3 separate failures).
                    circuit_breaker.record_failure(generation).await;
                    tracing::error!(
                        "⛔ {} [{}]: exhausted {} retries — giving up",
                        status.as_u16(),
                        reason,
                        max_retries
                    );
                    return Err(ProxyError::Upstream(format!(
                        "{} after {} retries ({}): {}",
                        status,
                        max_retries,
                        reason,
                        crate::str_utils::safe_truncate(&upstream_err.message, 300)
                    )));
                }
                let delay = delay_with_jitter(base_delay_ms, attempt);
                tracing::warn!(
                    "🔄 {} [{}] (attempt {}/{}) — retrying in {}ms",
                    status.as_u16(),
                    reason,
                    attempt,
                    max_retries,
                    delay
                );
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
                    _ = crate::SHUTDOWN_TOKEN.cancelled() => {
                        tracing::warn!("Server is draining — aborting retry backoff");
                        return Err(ProxyError::Upstream(
                            "Server is shutting down — request not retried".to_string(),
                        ));
                    }
                }
                continue;
            }
            ErrorClass::Fixable { reason } => {
                // Issue #63/#88: Tool schema format mismatch — strip tools and retry
                let error_msg = upstream_err.message.to_lowercase();
                if (error_msg.contains("missing field")
                    || error_msg.contains("deserialize")
                    || error_msg.contains("schema"))
                    && attempt <= MAX_RETRIES
                {
                    tracing::warn!(
                        "Tool schema mismatch (attempt {}/{}): {} -- retrying WITHOUT tools",
                        attempt,
                        MAX_RETRIES,
                        reason
                    );
                    openai_req.tools = None;
                    openai_req.tool_choice = None;
                    continue;
                }
                if attempt >= MAX_RETRIES {
                    // v10.2: If we exhausted retries on input_tokens overflow, it's truly full
                    if reason.contains("input_tokens overflow") {
                        return Err(ProxyError::ContextOverflow(format!(
                            "Context window full: {}. Use /compact to reduce context.",
                            crate::str_utils::safe_truncate(&upstream_err.message, 300)
                        )));
                    }
                    tracing::error!(
                        "⛔ Fixable [{}]: exhausted {} retries — giving up",
                        reason,
                        MAX_RETRIES
                    );
                    return Err(ProxyError::Upstream(format!(
                        "Fixable error after {} retries ({}): {}",
                        MAX_RETRIES,
                        reason,
                        crate::str_utils::safe_truncate(&upstream_err.message, 300)
                    )));
                }
                // FIX 4: Check for overflow loop pattern before retrying
                if reason.contains("input_tokens overflow") {
                    let real_input = error_regexes()
                        .input_tokens
                        .captures(&upstream_err.message)
                        .and_then(|c| c.get(1)?.as_str().parse::<u32>().ok())
                        .unwrap_or(0);
                    if real_input > 0
                        && OverflowLoopTracker::check_overflow_loop(&openai_req.model, real_input)
                    {
                        tracing::warn!(
                            "[LAUNCH] Overflow loop detected for model {} at ~{}K tokens — forcing ContextOverflow",
                            openai_req.model,
                            real_input / 1000
                        );
                        return Err(ProxyError::ContextOverflow(format!(
                            "Overflow loop detected: {} consecutive overflows at ~{}K tokens for {}. Use /compact to reduce context.",
                            3,
                            real_input / 1000,
                            openai_req.model
                        )));
                    }
                }
                // Issue #34 M9 / #62: precise extraction unified across both retry paths.
                let current = openai_req.max_tokens.unwrap_or(64000);
                openai_req.max_tokens = Some(clamp_max_tokens_for_retry(
                    status,
                    reason,
                    &upstream_err.message,
                    current,
                    attempt,
                    "",
                ));
                continue;
            }
            ErrorClass::Fatal { reason } => {
                tracing::error!(
                    "💀 {} [{}]: {}",
                    status.as_u16(),
                    reason,
                    crate::str_utils::safe_truncate(&upstream_err.message, 500)
                );
                // v6.1/v10.2: input_tokens overflow -> try to extract safe max_tokens
                if reason.contains("input_tokens overflow") {
                    return Err(ProxyError::ContextOverflow(format!(
                        "Context window full: {}. Use /compact to reduce context.",
                        crate::str_utils::safe_truncate(&upstream_err.message, 300)
                    )));
                }
                return Err(ProxyError::Upstream(format!(
                    "Fatal {} ({}): {}",
                    status,
                    reason,
                    crate::str_utils::safe_truncate(&upstream_err.message, 300)
                )));
            }
        }
    }
}

/// Resilient send for streaming: returns raw reqwest::Response (not parsed).
/// Same retry logic as resilient_send but returns the response for streaming.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn resilient_send_raw(
    client: &Client,
    config: &Config,
    openai_req: &mut openai::OpenAIRequest,
    upstream_name: &str,
    circuit_breaker: &crate::proxy::concurrency::CircuitBreaker,
    client_headers: &crate::proxy::headers::ClientHeaders,
) -> ProxyResult<reqwest::Response> {
    let mut attempt: u32 = 0;

    loop {
        attempt += 1;

        // S8: Fail-fast when server is draining — skip retries only, not first attempt
        if attempt > 1 && crate::IS_DRAINING.load(std::sync::atomic::Ordering::Relaxed) {
            tracing::warn!("Server is draining — skipping retry (attempt {})", attempt);
            return Err(crate::error::ProxyError::Upstream(
                "Server is shutting down — request not retried".to_string(),
            ));
        }

        // Circuit breaker check
        let (allowed, generation) = circuit_breaker.is_allowed().await;
        if !allowed {
            tracing::warn!("Circuit breaker OPEN — rejecting request (upstream unhealthy)");
            return Err(ProxyError::Upstream(
                "Service unavailable: circuit breaker open".to_string(),
            ));
        }
        let mut req_builder = client
            .post(config.get_upstream_url(upstream_name))
            .json(&*openai_req)
            // CR1-fix: No per-request timeout for streaming.
            // Protection: read_timeout(120s) on client + CHUNK_TIMEOUT in streaming.rs
            // CR2-fix: Force HTTP/1.1 — NIM sends HTTP/2 GOAWAY that kills long streams.
            // HTTP/1.1 uses a dedicated TCP connection with no multiplexing interference.
            .version(reqwest::Version::HTTP_11);

        if let Some(api_key) = &config.get_upstream_key(upstream_name) {
            req_builder = req_builder.header("Authorization", format!("Bearer {}", api_key));
        }

        // Issue #35: Forward anthropic-beta + anthropic-version to Anthropic upstreams
        if config.get_upstream_type(upstream_name) == UpstreamType::Anthropic {
            let beta = client_headers.resolve_anthropic_beta();
            let version = client_headers.resolve_anthropic_version();
            tracing::debug!(
                "📤 [stream] Forwarding to Anthropic upstream '{}': beta={}, version={}",
                upstream_name,
                beta,
                version
            );
            req_builder = req_builder.header("anthropic-beta", &beta);
            req_builder = req_builder.header("anthropic-version", version);
        }

        let response = req_builder.send().await?;
        let status = response.status();

        if status.is_success() {
            circuit_breaker.record_success(generation).await;
            OverflowLoopTracker::reset_tracker(&openai_req.model);
            if attempt > 1 {
                tracing::info!("🔄 Streaming request succeeded on attempt #{}", attempt);
            }
            return Ok(response);
        }

        let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());

        // === Smart Retry: 3-Layer Error Classification [stream] ===
        let upstream_err = parse_upstream_error(status.as_u16(), &error_text);
        tracing::debug!(
            "[SCAN] [stream] Parsed error: status={}, type={:?}, param={:?}, msg={}",
            upstream_err.status,
            upstream_err.error_type,
            upstream_err.param,
            crate::str_utils::safe_truncate(&upstream_err.message, 100)
        );

        let class = classify_error(&upstream_err);
        tracing::debug!("🧠 [stream] Classified: {:?} (status={})", class, status.as_u16());

        match class {
            ErrorClass::Retryable { base_delay_ms, max_retries, reason } => {
                if attempt >= max_retries {
                    // Only record ONE CB failure when ALL retries are exhausted.
                    // Internal retries are self-healing — they must not individually
                    // trip the breaker (3 retries of 1 request ≠ 3 separate failures).
                    circuit_breaker.record_failure(generation).await;
                    tracing::error!(
                        "⛔ [stream] {} [{}]: exhausted {} retries — giving up",
                        status.as_u16(),
                        reason,
                        max_retries
                    );
                    return Err(ProxyError::Upstream(format!(
                        "{} after {} retries ({}): {}",
                        status,
                        max_retries,
                        reason,
                        crate::str_utils::safe_truncate(&upstream_err.message, 300)
                    )));
                }
                let delay = delay_with_jitter(base_delay_ms, attempt);
                tracing::warn!(
                    "🔄 [stream] {} [{}] (attempt {}/{}) — retrying in {}ms",
                    status.as_u16(),
                    reason,
                    attempt,
                    max_retries,
                    delay
                );
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
                    _ = crate::SHUTDOWN_TOKEN.cancelled() => {
                        tracing::warn!("Server is draining — aborting retry backoff");
                        return Err(ProxyError::Upstream(
                            "Server is shutting down — request not retried".to_string(),
                        ));
                    }
                }
                continue;
            }
            ErrorClass::Fixable { reason } => {
                // Issue #63/#88: Tool schema format mismatch — strip tools and retry
                let error_msg = upstream_err.message.to_lowercase();
                if (error_msg.contains("missing field")
                    || error_msg.contains("deserialize")
                    || error_msg.contains("schema"))
                    && attempt <= MAX_RETRIES
                {
                    tracing::warn!(
                        "Tool schema mismatch (attempt {}/{}): {} -- retrying WITHOUT tools",
                        attempt,
                        MAX_RETRIES,
                        reason
                    );
                    openai_req.tools = None;
                    openai_req.tool_choice = None;
                    continue;
                }
                if attempt >= MAX_RETRIES {
                    // v10.2: If we exhausted retries on input_tokens overflow, it's truly full
                    if reason.contains("input_tokens overflow") {
                        return Err(ProxyError::ContextOverflow(format!(
                            "Context window full: {}. Use /compact to reduce context.",
                            crate::str_utils::safe_truncate(&upstream_err.message, 300)
                        )));
                    }
                    tracing::error!(
                        "⛔ [stream] Fixable [{}]: exhausted {} retries — giving up",
                        reason,
                        MAX_RETRIES
                    );
                    return Err(ProxyError::Upstream(format!(
                        "Fixable error after {} retries ({}): {}",
                        MAX_RETRIES,
                        reason,
                        crate::str_utils::safe_truncate(&upstream_err.message, 300)
                    )));
                }
                // FIX 4: Check for overflow loop pattern before retrying
                if reason.contains("input_tokens overflow") {
                    let real_input = error_regexes()
                        .input_tokens
                        .captures(&upstream_err.message)
                        .and_then(|c| c.get(1)?.as_str().parse::<u32>().ok())
                        .unwrap_or(0);
                    if real_input > 0
                        && OverflowLoopTracker::check_overflow_loop(&openai_req.model, real_input)
                    {
                        tracing::warn!(
                            "[LAUNCH] [stream] Overflow loop detected for model {} at ~{}K tokens — forcing ContextOverflow",
                            openai_req.model,
                            real_input / 1000
                        );
                        return Err(ProxyError::ContextOverflow(format!(
                            "Overflow loop detected: {} consecutive overflows at ~{}K tokens for {}. Use /compact to reduce context.",
                            3,
                            real_input / 1000,
                            openai_req.model
                        )));
                    }
                }
                // Issue #34 M9 / #62: precise extraction unified across both retry paths.
                let current = openai_req.max_tokens.unwrap_or(64000);
                openai_req.max_tokens = Some(clamp_max_tokens_for_retry(
                    status,
                    reason,
                    &upstream_err.message,
                    current,
                    attempt,
                    "[stream] ",
                ));
                continue;
            }
            ErrorClass::Fatal { reason } => {
                tracing::error!(
                    "💀 [stream] {} [{}]: {}",
                    status.as_u16(),
                    reason,
                    crate::str_utils::safe_truncate(&upstream_err.message, 500)
                );
                // v6.1/v10.2: input_tokens overflow -> 400 (CC won't retry)
                if reason.contains("input_tokens overflow") {
                    return Err(ProxyError::ContextOverflow(format!(
                        "Context window full: {}. Use /compact to reduce context.",
                        crate::str_utils::safe_truncate(&upstream_err.message, 300)
                    )));
                }
                return Err(ProxyError::Upstream(format!(
                    "Fatal {} ({}): {}",
                    status,
                    reason,
                    crate::str_utils::safe_truncate(&upstream_err.message, 300)
                )));
            }
        }
    }
}

#[cfg(test)]
mod anthropic_header_tests {
    use crate::proxy::headers::ClientHeaders;

    #[test]
    fn test_anthropic_headers_sent_to_anthropic_upstream() {
        // Verify that when upstream type is Anthropic, headers would be set
        let ch = ClientHeaders {
            anthropic_beta: Some("interleaved-thinking-2025-05-14".into()),
            anthropic_version: Some("2024-10-22".into()),
        };
        let beta = ch.resolve_anthropic_beta();
        let version = ch.resolve_anthropic_version();

        // Beta should include proxy minimum + client betas
        assert!(beta.contains("prompt-caching-scope-2026-01-05"));
        assert!(beta.contains("interleaved-thinking-2025-05-14"));

        // Version should be forwarded
        assert_eq!(version, "2024-10-22");
    }

    #[test]
    fn test_anthropic_headers_default_version() {
        // When client doesn't send anthropic-version, default is used
        let ch = ClientHeaders { anthropic_beta: None, anthropic_version: None };

        assert_eq!(ch.resolve_anthropic_version(), "2023-06-01");
        assert_eq!(ch.resolve_anthropic_beta(), "prompt-caching-scope-2026-01-05");
    }

    #[test]
    fn test_beta_merge_with_per_route_type() {
        // When global=NIM but bigmodel=Anthropic, only bigmodel gets betas
        // This is tested by get_upstream_type() — if it returns Anthropic,
        // the headers code runs. If it returns NIM, the headers code doesn't run.
        // Here we verify the header resolution logic itself.
        let ch = ClientHeaders {
            anthropic_beta: Some("compact-2026-01-12".into()),
            anthropic_version: None,
        };
        let beta = ch.resolve_anthropic_beta();

        assert!(beta.contains("prompt-caching-scope-2026-01-05"));
        assert!(beta.contains("compact-2026-01-12"));
    }
}

#[cfg(test)]
mod clamp_max_tokens_tests {
    use super::clamp_max_tokens_for_retry;
    use reqwest::StatusCode;

    #[test]
    fn non_overflow_halves_above_floor() {
        let got = clamp_max_tokens_for_retry(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit",
            "",
            64000,
            1,
            "",
        );
        assert_eq!(got, 32000);
    }

    #[test]
    fn halving_respects_min_clamp_floor() {
        // 5000 / 2 = 2500 -> floored to MIN_CLAMP_TOKENS (4096). Stream label is cosmetic.
        let got = clamp_max_tokens_for_retry(
            StatusCode::BAD_REQUEST,
            "too_large",
            "",
            5000,
            1,
            "[stream] ",
        );
        assert_eq!(got, 4096);
    }

    #[test]
    fn overflow_uses_precise_extraction_not_halving() {
        // input=100000, limit=200000 -> safe=99744; attempt=1 margin=3072 -> 96672.
        let msg = "You passed 100000 input tokens and requested 64000 output tokens. \
                   However, the model's context length is only 200000 tokens";
        let got = clamp_max_tokens_for_retry(
            StatusCode::BAD_REQUEST,
            "input_tokens overflow",
            msg,
            64000,
            1,
            "",
        );
        assert_eq!(got, 96672, "must use precise extraction, not crude halving");
        assert_ne!(got, 32000, "crude halving (64000/2) would be wrong here");
    }

    #[test]
    fn overflow_without_parseable_message_falls_back_to_halving() {
        let got = clamp_max_tokens_for_retry(
            StatusCode::BAD_REQUEST,
            "input_tokens overflow",
            "no parseable numbers here",
            64000,
            2,
            "",
        );
        assert_eq!(got, 32000);
    }
}
