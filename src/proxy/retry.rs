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

/// Issue #83 (P0): first-byte (response-headers) timeout.
///
/// reqwest's `read_timeout` only fires AFTER the first byte is received, so an upstream
/// that accepts the TLS connection but never sends a response (model stall, e.g. a dead
/// or overloaded NIM model) would hang the request forever. We wrap the upstream
/// `send()` future in this timeout: `send()` resolves once the response headers arrive,
/// so this bounds time-to-first-byte WITHOUT capping the total stream duration (which is
/// why CR1/CR2 removed the global `.timeout()` for streaming).
///
/// Default 60s — generous enough for a legitimate model cold start, bounded so a stall
/// can never block Claude Code indefinitely. Configurable via
/// `UPSTREAM_FIRST_BYTE_TIMEOUT_SECS` (must be > 0; invalid/zero falls back to default).
pub(crate) fn first_byte_timeout_secs() -> u64 {
    std::env::var("UPSTREAM_FIRST_BYTE_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&v| v > 0)
        .unwrap_or(60)
}

/// Issue #67: priority-ordered fallback models (comma-separated upstream model ids in
/// `FALLBACK_MODELS`). When the primary model fails with a non-transient error
/// (5xx / model-not-found / EOL / stall — NOT 429 or auth), NEXUS retries the request
/// against the next model here before giving up, so one dead model cannot take down every
/// session mapped to it. Empty (default) = no fallback.
pub(crate) fn fallback_models() -> Vec<String> {
    std::env::var("FALLBACK_MODELS")
        .ok()
        .map(|s| s.split(',').map(|m| m.trim().to_string()).filter(|m| !m.is_empty()).collect())
        .unwrap_or_default()
}

/// Max fallback models to try per request (Issue #67: "max 2 fallbacks").
pub(crate) const MAX_FALLBACKS: usize = 2;

/// Whether an upstream HTTP status is fallback-eligible: a non-transient signal that THIS
/// model is unavailable and another might work. 429 (rate limit) is Retryable elsewhere;
/// 401/403 (auth) and 400 (bad request / context overflow) are config/input errors that a
/// different model cannot fix.
pub(crate) fn is_fallback_eligible_status(status: u16) -> bool {
    matches!(status, 500 | 502 | 503 | 504 | 404 | 410)
}

/// Issue #67: swap `openai_req.model` to the next fallback (within `MAX_FALLBACKS`).
/// Returns the new model name when a swap happened, or None when fallbacks are exhausted.
fn try_next_fallback(
    openai_req: &mut openai::OpenAIRequest,
    fallback_used: &mut usize,
) -> Option<String> {
    if *fallback_used >= MAX_FALLBACKS {
        return None;
    }
    let next = fallback_models().get(*fallback_used)?.clone();
    *fallback_used += 1;
    let prev = std::mem::replace(&mut openai_req.model, next.clone());
    tracing::warn!(
        "🔀 [fallback] model '{}' failed — falling back to '{}' (fallback {}/{})",
        prev,
        next,
        *fallback_used,
        MAX_FALLBACKS
    );
    Some(next)
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
            // Growing safety margin to absorb NIM re-tokenization drift (~257 tokens/retry).
            // The 1024 floor is intentionally below MIN_CLAMP_TOKENS: `safe` is already
            // floored at MIN_CLAMP_TOKENS inside extract_safe_max_tokens_from_error, so the
            // margin must be allowed to push the result below it; otherwise a near-full
            // context (safe ≈ MIN_CLAMP) would retry at ~the same value and re-overflow.
            // This sub-floor gives the re-tokenization margin real headroom to fit.
            let margin = 2048 + (attempt * 1024);
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
    let mut fallback_used: usize = 0; // Issue #67: fallback models tried so far

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

        let response = match tokio::time::timeout(
            Duration::from_secs(first_byte_timeout_secs()),
            req_builder.send(),
        )
        .await
        {
            Ok(send_result) => send_result?,
            Err(_elapsed) => {
                // Issue #83 (P0): upstream stalled (no response headers within the first-byte
                // budget). The 300s builder timeout would still bound this eventually, but fail
                // fast so a stalled model can't tie up the request for a full 5 minutes.
                circuit_breaker.record_failure(generation).await;
                tracing::error!(
                    "⛔ first-byte timeout: upstream '{}' sent no response within {}s (model stall)",
                    upstream_name,
                    first_byte_timeout_secs()
                );
                // Issue #67: a stall means THIS model is unavailable — try a fallback model.
                if try_next_fallback(openai_req, &mut fallback_used).is_some() {
                    attempt = 0;
                    continue;
                }
                return Err(ProxyError::Upstream(format!(
                    "Upstream '{}' stalled: no response within {}s (model unavailable)",
                    upstream_name,
                    first_byte_timeout_secs()
                )));
            }
        };
        let status = response.status();

        if status.is_success() {
            // Issue #119: read the body first so a degenerate/empty 200 (which NIM returns
            // when input+max_tokens leave no room) becomes a clear, non-retriable
            // ContextOverflow (400 -> /compact) instead of an opaque
            // "502 HTTP error: error decoding response body".
            let body = response.bytes().await?;
            let resp: openai::OpenAIResponse = serde_json::from_slice(&body).map_err(|e| {
                let body_str = String::from_utf8_lossy(&body);
                let preview = crate::str_utils::safe_truncate(&body_str, 300);
                tracing::error!(
                    "[DECODE] non-streaming 200 body undecodable: {e} | body: {preview}"
                );
                ProxyError::ContextOverflow(
                    "Upstream returned an undecodable response (likely empty — the context is \
                     too full to leave room for output). Use /compact to reduce context."
                        .to_string(),
                )
            })?;
            if resp.choices.is_empty() {
                tracing::warn!(
                    "[DECODE] non-streaming upstream returned empty choices (degenerate 200 — \
                     context too full for model {})",
                    openai_req.model
                );
                return Err(ProxyError::ContextOverflow(
                    "Upstream returned an empty response (no choices) — the request left no room \
                     for output. Use /compact to reduce context."
                        .to_string(),
                ));
            }
            // Only a genuinely usable body counts as success (Issue #119): record health and
            // clear overflow history AFTER validation — never on a degenerate/empty 200.
            circuit_breaker.record_success(generation).await;
            OverflowLoopTracker::reset_tracker(&openai_req.model);
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
                // Issue #67: a 5xx is a model-availability signal. If a fallback model is
                // configured, switch to it on the first such failure instead of burning the
                // full (slow, backoff-growing) retry budget on a likely-dead model — CC would
                // time out first. Falls through to the normal retry/exhaust path when no
                // fallback is configured or all fallbacks have been used.
                if is_fallback_eligible_status(status.as_u16())
                    && try_next_fallback(openai_req, &mut fallback_used).is_some()
                {
                    attempt = 0;
                    continue;
                }
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
                // Issue #67: a non-transient failure (5xx / model-not-found / EOL) means THIS
                // model is unavailable — try a fallback before giving up. Auth (401/403) and
                // bad-request/overflow (400) are excluded: a different model cannot fix them.
                if is_fallback_eligible_status(status.as_u16())
                    && try_next_fallback(openai_req, &mut fallback_used).is_some()
                {
                    attempt = 0;
                    continue;
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
    let mut fallback_used: usize = 0; // Issue #67: fallback models tried so far

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

        let response = match tokio::time::timeout(
            Duration::from_secs(first_byte_timeout_secs()),
            req_builder.send(),
        )
        .await
        {
            Ok(send_result) => send_result?,
            Err(_elapsed) => {
                // Issue #83 (P0): the upstream accepted the connection but sent no response
                // headers within the first-byte budget (model stall). Without this the stream
                // hangs forever — reqwest's read_timeout only fires AFTER the first byte.
                // Record a CB failure so a persistently-stalled model trips the breaker.
                circuit_breaker.record_failure(generation).await;
                tracing::error!(
                    "⛔ [stream] first-byte timeout: upstream '{}' sent no response within {}s (model stall)",
                    upstream_name,
                    first_byte_timeout_secs()
                );
                // Issue #67: a stall means THIS model is unavailable — try a fallback model.
                if try_next_fallback(openai_req, &mut fallback_used).is_some() {
                    attempt = 0;
                    continue;
                }
                return Err(ProxyError::Upstream(format!(
                    "Upstream '{}' stalled: no response within {}s (model unavailable)",
                    upstream_name,
                    first_byte_timeout_secs()
                )));
            }
        };
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
                // Issue #67: a 5xx is a model-availability signal. If a fallback model is
                // configured, switch to it on the first such failure instead of burning the
                // full (slow, backoff-growing) retry budget on a likely-dead model — CC would
                // time out first. Falls through to the normal retry/exhaust path when no
                // fallback is configured or all fallbacks have been used.
                if is_fallback_eligible_status(status.as_u16())
                    && try_next_fallback(openai_req, &mut fallback_used).is_some()
                {
                    attempt = 0;
                    continue;
                }
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
                // Issue #67: a non-transient failure (5xx / model-not-found / EOL) means THIS
                // model is unavailable — try a fallback before giving up. Auth (401/403) and
                // bad-request/overflow (400) are excluded: a different model cannot fix them.
                if is_fallback_eligible_status(status.as_u16())
                    && try_next_fallback(openai_req, &mut fallback_used).is_some()
                {
                    attempt = 0;
                    continue;
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

    #[test]
    fn overflow_margin_may_clamp_below_min_clamp_floor() {
        // Intentional (regression guard): `safe` is already floored at MIN_CLAMP_TOKENS by
        // extraction, so the re-tokenization margin is allowed to push the retry BELOW
        // MIN_CLAMP (floor is 1024, not MIN_CLAMP_TOKENS) to actually fit a near-full
        // context. input=126976, limit=131072 -> safe=max(3840,4096)=4096; attempt=2 ->
        // margin=4096 -> 4096-4096=0 -> floored to 1024. Raising this floor to MIN_CLAMP
        // would make the retry request ~the same budget and re-overflow.
        let msg = "You passed 126976 input tokens and requested 64000 output tokens. \
                   However, the model's context length is only 131072 tokens";
        let got = clamp_max_tokens_for_retry(
            StatusCode::BAD_REQUEST,
            "input_tokens overflow",
            msg,
            64000,
            2,
            "",
        );
        assert_eq!(got, 1024, "margin must be able to push below MIN_CLAMP_TOKENS to fit");
    }
}

#[cfg(test)]
mod first_byte_timeout_tests {
    use super::first_byte_timeout_secs;

    /// Save/restore the single env var this test touches so it is order-independent.
    fn with_env<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
        let key = "UPSTREAM_FIRST_BYTE_TIMEOUT_SECS";
        let prev = std::env::var(key).ok();
        match value {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        let out = f();
        match prev {
            Some(p) => std::env::set_var(key, p),
            None => std::env::remove_var(key),
        }
        out
    }

    #[test]
    fn default_and_overrides() {
        // Default 60s when unset.
        assert_eq!(with_env(None, first_byte_timeout_secs), 60);
        // Honoured when valid.
        assert_eq!(with_env(Some("30"), first_byte_timeout_secs), 30);
        // Zero and invalid fall back to the default — a 0s budget would abort every request.
        assert_eq!(with_env(Some("0"), first_byte_timeout_secs), 60);
        assert_eq!(with_env(Some("garbage"), first_byte_timeout_secs), 60);
    }
}

#[cfg(test)]
mod fallback_tests {
    use super::{fallback_models, is_fallback_eligible_status, try_next_fallback, MAX_FALLBACKS};
    use crate::models::openai::OpenAIRequest;

    fn with_fallbacks<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
        let key = "FALLBACK_MODELS";
        let prev = std::env::var(key).ok();
        match value {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        let out = f();
        match prev {
            Some(p) => std::env::set_var(key, p),
            None => std::env::remove_var(key),
        }
        out
    }

    fn req(model: &str) -> OpenAIRequest {
        OpenAIRequest {
            model: model.to_string(),
            messages: vec![],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop: None,
            stream: None,
            stream_options: None,
            tools: None,
            tool_choice: None,
            chat_template_kwargs: None,
            response_format: None,
        }
    }

    #[test]
    fn eligible_status_only_non_transient() {
        for s in [500, 502, 503, 504, 404, 410] {
            assert!(is_fallback_eligible_status(s), "{s} should be fallback-eligible");
        }
        // 429 (retryable), auth (401/403), bad-request/overflow (400), success (200) must NOT.
        for s in [200, 400, 401, 403, 429] {
            assert!(!is_fallback_eligible_status(s), "{s} must NOT be fallback-eligible");
        }
    }

    // All FALLBACK_MODELS-dependent assertions run in one test so the process-wide env var is
    // never mutated concurrently by a sibling test.
    #[test]
    fn fallback_parsing_and_swap() {
        // Default: no fallbacks.
        assert_eq!(with_fallbacks(None, fallback_models), Vec::<String>::new());
        // Parse, trim, and drop empty entries.
        assert_eq!(
            with_fallbacks(Some("a, b ,, c"), fallback_models),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        // Swaps in priority order and stops at MAX_FALLBACKS (2) even if more are configured.
        with_fallbacks(Some("m1,m2,m3"), || {
            let mut r = req("primary");
            let mut used = 0usize;
            assert_eq!(try_next_fallback(&mut r, &mut used).as_deref(), Some("m1"));
            assert_eq!(r.model, "m1");
            assert_eq!(try_next_fallback(&mut r, &mut used).as_deref(), Some("m2"));
            assert_eq!(r.model, "m2");
            assert_eq!(MAX_FALLBACKS, 2);
            assert_eq!(try_next_fallback(&mut r, &mut used), None, "must stop at MAX_FALLBACKS");
            assert_eq!(r.model, "m2", "model unchanged once fallbacks are exhausted");
        });
        // No fallbacks configured -> None, model untouched.
        with_fallbacks(None, || {
            let mut r = req("primary");
            let mut used = 0usize;
            assert_eq!(try_next_fallback(&mut r, &mut used), None);
            assert_eq!(r.model, "primary");
        });
    }
}
