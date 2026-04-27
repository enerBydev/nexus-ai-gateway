use std::time::Duration;

use reqwest::Client;

use crate::config::{Config, UpstreamType};
use crate::error::{ProxyError, ProxyResult};
use crate::models::openai;
use crate::proxy::classify::{
    classify_error, delay_with_jitter, error_regexes, ErrorClass, MAX_RETRIES, MIN_CLAMP_TOKENS,
};
use crate::proxy::error_types::parse_upstream_error;
use crate::proxy::overflow_tracker::OverflowLoopTracker;

// v0.11.0: Stream stability constants (CR-01, CR-02)
pub(crate) const CHUNK_TIMEOUT_SECS: u64 = 120; // Max seconds to wait for next SSE chunk from NIM
pub(crate) const MAX_SSE_BUFFER: usize = 10 * 1024 * 1024; // 10MB safety limit for SSE buffer

/// Resilient send for non-streaming: returns parsed OpenAI response.
/// Auto-retries on 429 (rate limit) with exponential backoff.
/// Auto-clamps max_tokens on 400 (too large) and retries.
pub(crate) async fn resilient_send(
    client: &Client,
    config: &Config,
    openai_req: &mut openai::OpenAIRequest,
    upstream_name: &str,
    circuit_breaker: &crate::proxy::concurrency::CircuitBreaker,
) -> ProxyResult<openai::OpenAIResponse> {
    let mut attempt: u32 = 0;

    loop {
        attempt += 1;

        // Circuit breaker check
        let (allowed, generation) = circuit_breaker.is_allowed().await;
        if !allowed {
            tracing::warn!("⚡ Circuit breaker OPEN — rejecting request (upstream unhealthy)");
            return Err(ProxyError::Upstream(
                "Service unavailable: circuit breaker open".to_string(),
            ));
        }

        let mut req_builder = client
            .post(config.get_upstream_url(upstream_name))
            .json(&*openai_req)
            .timeout(Duration::from_secs(900));

        if let Some(api_key) = &config.get_upstream_key(upstream_name) {
            req_builder = req_builder.header("Authorization", format!("Bearer {}", api_key));
        }

        // v0.13.0: Only send anthropic-beta to Anthropic endpoints
        if config.get_upstream_type(upstream_name) == UpstreamType::Anthropic {
            req_builder = req_builder.header("anthropic-beta", "prompt-caching-2024-06-01");
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
            "🔍 Parsed error: status={}, type={:?}, param={:?}, msg={}",
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
                tokio::time::sleep(Duration::from_millis(delay)).await;
                continue;
            }
            ErrorClass::Fixable { reason } => {
                if attempt >= MAX_RETRIES {
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
                            "🚀 Overflow loop detected for model {} at ~{}K tokens — forcing ContextOverflow",
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
                let current = openai_req.max_tokens.unwrap_or(64000);
                let new_max = (current / 2).max(MIN_CLAMP_TOKENS);
                tracing::warn!(
                    "🔧 {} [{}] (attempt {}/{}): clamping max_tokens {} → {}",
                    status.as_u16(),
                    reason,
                    attempt,
                    MAX_RETRIES,
                    current,
                    new_max
                );
                openai_req.max_tokens = Some(new_max);
                continue;
            }
            ErrorClass::Fatal { reason } => {
                tracing::error!(
                    "💀 {} [{}]: {}",
                    status.as_u16(),
                    reason,
                    crate::str_utils::safe_truncate(&upstream_err.message, 500)
                );
                // v6.1/v10.2: input_tokens overflow → try to extract safe max_tokens
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
pub(crate) async fn resilient_send_raw(
    client: &Client,
    config: &Config,
    openai_req: &mut openai::OpenAIRequest,
    upstream_name: &str,
    circuit_breaker: &crate::proxy::concurrency::CircuitBreaker,
) -> ProxyResult<reqwest::Response> {
    let mut attempt: u32 = 0;

    loop {
        attempt += 1;

        // Circuit breaker check
        let (allowed, generation) = circuit_breaker.is_allowed().await;
        if !allowed {
            tracing::warn!("⚡ Circuit breaker OPEN — rejecting request (upstream unhealthy)");
            return Err(ProxyError::Upstream(
                "Service unavailable: circuit breaker open".to_string(),
            ));
        }
        let mut req_builder = client
            .post(config.get_upstream_url(upstream_name))
            .json(&*openai_req)
            .timeout(Duration::from_secs(900));

        if let Some(api_key) = &config.get_upstream_key(upstream_name) {
            req_builder = req_builder.header("Authorization", format!("Bearer {}", api_key));
        }

        // v0.13.0: Only send anthropic-beta to Anthropic endpoints
        if config.get_upstream_type(upstream_name) == UpstreamType::Anthropic {
            req_builder = req_builder.header("anthropic-beta", "prompt-caching-2024-06-01");
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
            "🔍 [stream] Parsed error: status={}, type={:?}, param={:?}, msg={}",
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
                tokio::time::sleep(Duration::from_millis(delay)).await;
                continue;
            }
            ErrorClass::Fixable { reason } => {
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
                            "🚀 [stream] Overflow loop detected for model {} at ~{}K tokens — forcing ContextOverflow",
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
                // v10.2: For input_tokens overflow, calculate exact safe max_tokens
                let new_max = if reason.contains("input_tokens overflow") {
                    if let Some(safe) = crate::proxy::classify::extract_safe_max_tokens_from_error(
                        &upstream_err.message,
                    ) {
                        // v0.11.0: Subtract safety margin to absorb NIM re-tokenization drift
                        // NIM adds ~257 tokens per retry (chat template expansion).
                        // Without margin: attempt1=63743, NIM says input=139010 (+257) → fail
                        // With margin: attempt1=61695, NIM says input=139010 → still fits
                        let margin = 2048 + (attempt * 1024); // Growing margin per retry
                        let safe_with_margin = safe.saturating_sub(margin).max(1024);
                        tracing::warn!(
                            "🔧 [stream] input_tokens overflow (attempt {}/{}): NIM safe={}, margin={}, clamping max_tokens → {}",
                            attempt, MAX_RETRIES, safe, margin, safe_with_margin
                        );
                        safe_with_margin
                    } else {
                        let current = openai_req.max_tokens.unwrap_or(64000);
                        (current / 2).max(MIN_CLAMP_TOKENS)
                    }
                } else {
                    let current = openai_req.max_tokens.unwrap_or(64000);
                    let halved = (current / 2).max(MIN_CLAMP_TOKENS);
                    tracing::warn!(
                        "🔧 [stream] {} [{}] (attempt {}/{}): clamping max_tokens {} → {}",
                        status.as_u16(),
                        reason,
                        attempt,
                        MAX_RETRIES,
                        current,
                        halved
                    );
                    halved
                };
                openai_req.max_tokens = Some(new_max);
                continue;
            }
            ErrorClass::Fatal { reason } => {
                tracing::error!(
                    "💀 [stream] {} [{}]: {}",
                    status.as_u16(),
                    reason,
                    crate::str_utils::safe_truncate(&upstream_err.message, 500)
                );
                // v6.1/v10.2: input_tokens overflow → 400 (CC won't retry)
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
