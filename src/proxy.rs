#[allow(unused_imports)] // v0.12.0: Circuit breaker integration pending
use crate::circuit_breaker::{self, CircuitState};
use crate::config::{Config, SharedConfig};
use crate::error::{ProxyError, ProxyResult};
use crate::models::{anthropic, openai};
use crate::tokenizer;
use crate::transform;
use crate::web_fetch;
use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue},
    response::{IntoResponse, Response},
    Extension, Json,
};
use bytes::Bytes;
use futures::stream::{Stream, StreamExt};
use reqwest::Client;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{RwLock as AsyncRwLock, Semaphore};

const MAX_RETRIES: u32 = 3; // Reduced from 4: less cascade amplification (1.58x→~1.3x)
#[allow(dead_code)]
const RETRY_BASE_MS: u64 = 1500; // Kept for reference, overridden per error class
const MIN_CLAMP_TOKENS: u32 = 4096;

// v0.11.0: Stream stability constants (CR-01, CR-02)
const CHUNK_TIMEOUT_SECS: u64 = 120; // Max seconds to wait for next SSE chunk from NIM
const MAX_SSE_BUFFER: usize = 10 * 1024 * 1024; // 10MB safety limit for SSE buffer

// ─── Smart Retry Infrastructure (3-Layer Error Classification) ─────────

/// Parsed NIM/OpenAI error response with structured fields
#[derive(Debug, Default)]
struct UpstreamError {
    status: u16,
    message: String,
    error_type: Option<String>, // NIM: "BadRequestError", etc.
    param: Option<String>,      // NIM: "input_tokens", etc.
    #[allow(dead_code)]
    code: Option<String>, // NIM: "400", etc.
}

/// Parse NIM/OpenAI error response to extract structured error info.
/// Handles nested errors where NIM wraps a 400 inside a 502.
fn parse_upstream_error(status: u16, body: &str) -> UpstreamError {
    let mut err = UpstreamError {
        status,
        message: body.to_string(),
        ..Default::default()
    };

    if let Ok(json) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(error_obj) = json.get("error") {
            err.message = error_obj
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or(body)
                .to_string();
            err.error_type = error_obj
                .get("type")
                .and_then(|v| v.as_str())
                .map(String::from);
            err.param = error_obj
                .get("param")
                .and_then(|v| v.as_str())
                .map(String::from);
            err.code = error_obj.get("code").and_then(|v| match v {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Number(n) => Some(n.to_string()),
                _ => None,
            });

            // NIM wraps errors: "Upstream returned 400 Bad Request: {...}"
            if let Some(inner) = extract_nested_error(&err.message) {
                tracing::debug!(
                    "🔍 Unwrapped nested error: {} → {}",
                    &err.message.chars().take(80).collect::<String>(),
                    &inner.message.chars().take(80).collect::<String>()
                );
                err.message = inner.message;
                if err.error_type.is_none() {
                    err.error_type = inner.error_type;
                }
                if err.param.is_none() {
                    err.param = inner.param;
                }
            }
        }
    }

    err
}

/// Extract nested error from NIM's wrapper format.
/// NIM sends: "Upstream returned 400 Bad Request: {\"status\":400,\"detail\":\"...\"}"
fn extract_nested_error(msg: &str) -> Option<UpstreamError> {
    let json_start = msg.find('{')?;
    let json_str = &msg[json_start..];
    let json: serde_json::Value = serde_json::from_str(json_str).ok()?;

    Some(UpstreamError {
        status: json.get("status").and_then(|v| v.as_u64()).unwrap_or(0) as u16,
        message: json
            .get("detail")
            .or_else(|| json.get("message"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        error_type: json.get("type").and_then(|v| v.as_str()).map(String::from),
        param: None,
        code: None,
    })
}

// ═══════════════════════════════════════════════════════════════════════════
// Rate Limit Detection System (Gap #7)
// ═══════════════════════════════════════════════════════════════════════════

/// L2 rate limit patterns from various providers
const L2_RATE_LIMIT_PATTERNS: &[&str] = &[
    "NIM concurrency cap (L2)",
    "concurrency limit exceeded",
    // REMOVED: "rate limit exceeded" - too broad, captures non-L2 rate limits
    "too many concurrent requests",
    "concurrent request limit",   // Alternative pattern
    "simultaneous request limit", // Alternative pattern
];

/// Check if error indicates L2 rate limit (provider-side concurrency)
fn is_l2_rate_limit(error: &UpstreamError) -> bool {
    L2_RATE_LIMIT_PATTERNS.iter().any(|pattern| {
        error
            .message
            .to_lowercase()
            .contains(&pattern.to_lowercase())
    })
}

/// L2 rate limit backoff configuration
#[allow(dead_code)] // v0.12.0: L2 backoff integration pending
const L2_BACKOFF_MULTIPLIER: f64 = 2.0;
#[allow(dead_code)] // v0.12.0: L2 backoff integration pending
const L2_MIN_BACKOFF_MS: u64 = 2000;
#[allow(dead_code)] // v0.12.0: L2 backoff integration pending
const L2_MAX_BACKOFF_MS: u64 = 30000;
#[allow(dead_code)] // v0.12.0: L2 backoff integration pending
const L2_JITTER_PERCENT: f64 = 0.25;

/// Log L2 rate limit with actionable information
#[allow(dead_code)] // v0.12.0: L2 backoff integration pending
fn log_l2_rate_limit(model: &str, error: &UpstreamError) {
    tracing::warn!(
        target: "nexus::rate_limit",
        "🚫 L2_RATE_LIMIT model={} message=\"{}\"",
        model,
        error.message.chars().take(200).collect::<String>()
    );
    tracing::info!(
        target: "nexus::metrics",
        "METRIC rate_limit_l2_total{{model=\"{}\"}} 1",
        model
    );
}

/// Error classification: 3 layers (Structural → Content-Aware → Status-Based)
#[derive(Debug)]
enum ErrorClass {
    /// Transient server error — retry with exponential backoff + jitter
    Retryable {
        base_delay_ms: u64,
        max_retries: u32,
        reason: &'static str,
    },
    /// Parameter error — auto-correct (reduce max_tokens) and retry
    Fixable { reason: &'static str },
    /// Fatal error — return immediately to CC with Anthropic-native error type
    Fatal { reason: &'static str },
}

/// Content patterns that indicate a fixable error (max_tokens/context overflow)
const FIXABLE_PATTERNS: &[&str] = &[
    "max_tokens",
    "context_length",
    "too large",
    "maximum context length",
    "context window",
    "token limit",
    "exceeds the model",
    "input tokens",
    "reduce the length",
];

/// Content patterns that indicate a transient/retryable condition
const RETRYABLE_PATTERNS: &[&str] = &[
    "temporarily unavailable",
    "service unavailable",
    "model is loading",
    "try again",
    "capacity",
    "overloaded",
    "timeout",
    "connection reset",
    "econnreset",
    "upstream connect error",
    "degraded function",
    "cannot be invoked",
];

/// Content patterns that indicate a fatal/permanent error
const FATAL_PATTERNS: &[&str] = &[
    "invalid api key",
    "unauthorized",
    "forbidden",
    "not found",
    "invalid model",
    "does not exist",
    "deprecated",
    "billing",
    "quota exceeded",
];

/// v10.2: Extract safe max_tokens from NIM input_tokens overflow error.
///
/// NIM error format: "You passed {input} input tokens and requested {output} output tokens.
///                    However, the model's context length is only {limit} tokens"
/// Returns: context_limit - real_input - safety_margin (256 tokens)
fn extract_safe_max_tokens_from_error(message: &str) -> Option<u32> {
    let re_input = regex::Regex::new(r"passed\s+(\d+)\s+input\s+tokens").ok()?;
    let re_limit = regex::Regex::new(r"context\s+length\s+is\s+only\s+(\d+)").ok()?;

    let real_input: u32 = re_input.captures(message)?.get(1)?.as_str().parse().ok()?;
    let context_limit: u32 = re_limit.captures(message)?.get(1)?.as_str().parse().ok()?;

    // Safety margin: 256 tokens to guarantee fit across tokenizer differences
    let safe_max = context_limit
        .saturating_sub(real_input)
        .saturating_sub(256)
        .max(MIN_CLAMP_TOKENS);

    tracing::info!(
        "📐 Extracted from NIM error: input={}, limit={}, safe_max={}",
        real_input,
        context_limit,
        safe_max
    );
    Some(safe_max)
}

/// 3-layer error classification.
///
/// Layer 0: Structural — uses NIM's typed error fields (error.type, error.param)
/// Layer 1: Content-Aware — pattern matching on error message body
/// Layer 2: Status-Based — HTTP status code fallback
fn classify_error(upstream: &UpstreamError) -> ErrorClass {
    let lower = upstream.message.to_lowercase();

    // Check for L2 rate limit first (more specific)
    if is_l2_rate_limit(upstream) {
        tracing::warn!(
            "⚠️ L2 rate limit detected: {} - using extended backoff",
            upstream.message.chars().take(100).collect::<String>()
        );
        return ErrorClass::Retryable {
            base_delay_ms: L2_MIN_BACKOFF_MS,
            max_retries: 3,
            reason: "L2 rate limit — provider concurrency cap",
        };
    }

    // ╔══════════════════════════════════════════════════════════╗
    // ║  LAYER 0: Structural — NIM typed error fields            ║
    // ╚══════════════════════════════════════════════════════════╝
    if let Some(ref etype) = upstream.error_type {
        if etype.as_str() == "BadRequestError" && upstream.param.as_deref() == Some("input_tokens")
        {
            // v10.2: Try to auto-fix by reducing max_tokens to fit real input
            // Only fatal if we can't extract the real counts from the error
            if let Some(_safe_max) = extract_safe_max_tokens_from_error(&upstream.message) {
                return ErrorClass::Fixable {
                    reason: "input_tokens overflow — auto-clamping max_tokens to fit (L0)",
                };
            }
            return ErrorClass::Fatal {
                reason: "input_tokens overflow — context full, needs /compact (L0)",
            };
        }
    }

    // ╔══════════════════════════════════════════════════════════╗
    // ║  LAYER 1: Content-Aware Classification                   ║
    // ╚══════════════════════════════════════════════════════════╝
    for pattern in FATAL_PATTERNS {
        if lower.contains(pattern) {
            return ErrorClass::Fatal {
                reason: "fatal pattern in error body (L1)",
            };
        }
    }
    for pattern in FIXABLE_PATTERNS {
        if lower.contains(pattern) {
            return ErrorClass::Fixable {
                reason: "fixable pattern — token/context overflow (L1)",
            };
        }
    }
    for pattern in RETRYABLE_PATTERNS {
        if lower.contains(pattern) {
            return ErrorClass::Retryable {
                base_delay_ms: 2000,
                max_retries: 3,
                reason: "retryable pattern in error body (L1)",
            };
        }
    }

    // ╔══════════════════════════════════════════════════════════╗
    // ║  LAYER 2: Status-Based Classification                    ║
    // ╚══════════════════════════════════════════════════════════╝
    match upstream.status {
        429 => ErrorClass::Retryable {
            base_delay_ms: 10_000, // 10s — matches NIM recovery time
            max_retries: 3,
            reason: "429 rate limit — NIM concurrency cap (L2)",
        },
        500 => ErrorClass::Retryable {
            base_delay_ms: 3000,
            max_retries: 3,
            reason: "500 internal server error (L2)",
        },
        502 => ErrorClass::Retryable {
            base_delay_ms: 3000,
            max_retries: 3,
            reason: "502 bad gateway (L2)",
        },
        503 => ErrorClass::Retryable {
            base_delay_ms: 5000,
            max_retries: 4,
            reason: "503 service unavailable — model loading (L2)",
        },
        504 => ErrorClass::Retryable {
            base_delay_ms: 4000,
            max_retries: 3,
            reason: "504 gateway timeout (L2)",
        },
        529 => ErrorClass::Retryable {
            base_delay_ms: 5000,
            max_retries: 3,
            reason: "529 overloaded (L2)",
        },
        400 => ErrorClass::Fatal {
            reason: "400 bad request — no fixable pattern (L2)",
        },
        401 => ErrorClass::Fatal {
            reason: "401 unauthorized (L2)",
        },
        402 => ErrorClass::Fatal {
            reason: "402 billing error (L2)",
        },
        403 => ErrorClass::Fatal {
            reason: "403 forbidden (L2)",
        },
        404 => ErrorClass::Fatal {
            reason: "404 not found (L2)",
        },
        405 => ErrorClass::Fatal {
            reason: "405 method not allowed (L2)",
        },
        413 => ErrorClass::Fixable {
            reason: "413 payload too large (L2)",
        },
        422 => ErrorClass::Fatal {
            reason: "422 unprocessable entity (L2)",
        },
        // v0.11.0 (HI-04): HTTP 408 is a timeout — should be retried, not fatal
        408 => ErrorClass::Retryable {
            base_delay_ms: 5000,
            max_retries: 3,
            reason: "408 request timeout (L2)",
        },
        406..=407 | 409..=412 | 414..=421 | 423..=499 => ErrorClass::Fatal {
            reason: "unknown 4xx client error (L2)",
        },
        501 | 505..=528 | 530..=599 => ErrorClass::Retryable {
            base_delay_ms: 2000,
            max_retries: 2,
            reason: "unknown 5xx server error (L2)",
        },
        _ => ErrorClass::Fatal {
            reason: "unexpected status code (L2)",
        },
    }
}

/// Calculate delay with exponential backoff + jitter (avoids thundering herd)
/// Jitter range: ±25% of base, capped at 30s
fn delay_with_jitter(base_ms: u64, attempt: u32) -> u64 {
    use rand::Rng;
    let exponential = base_ms * 2u64.pow(attempt.saturating_sub(1));
    let capped = exponential.min(30_000);
    let jitter_range = capped / 4;
    let jitter = rand::thread_rng().gen_range(0..=(jitter_range * 2));
    capped.saturating_sub(jitter_range) + jitter
}

// ─── Auto-Discovery: Dynamic Context Limits (Doc1) ─────────────────

/// Dynamically discovered model capabilities from NIM
#[derive(Debug, Clone)]
pub struct ModelCapabilities {
    max_total_tokens: u32,
    probed_at: std::time::Instant,
}

/// Cache for model capabilities, populated by probing NIM
pub type ModelCache = Arc<AsyncRwLock<HashMap<String, ModelCapabilities>>>;

const DEFAULT_CONTEXT_LIMIT: u32 = 131_072;
const CACHE_TTL_SECS: u64 = 3600; // re-probe every hour

/// Probe NIM to discover a model's max_total_tokens.
/// Technique: send max_tokens=999999 → NIM returns error revealing real limit.
async fn probe_model_limit(
    client: &Client,
    base_url: &str,
    api_key: Option<&str>,
    model: &str,
) -> Option<u32> {
    let url = format!("{}/v1/chat/completions", base_url.trim_end_matches('/'));

    let probe_body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 999999
    });

    let mut req_builder = client
        .post(&url)
        .header("Content-Type", "application/json")
        // v0.12.0: Prompt caching header (Gap #1)
        .header("anthropic-beta", "prompt-caching-2024-06-01")
        .json(&probe_body)
        .timeout(Duration::from_secs(15));

    if let Some(key) = api_key {
        req_builder = req_builder.header("Authorization", format!("Bearer {}", key));
    }

    let resp = req_builder.send().await.ok()?;
    let body = resp.text().await.ok()?;

    // Parse "max_total_tokens=262144" from NIM error message
    let re = regex::Regex::new(r"max_total_tokens=(\d+)").ok()?;
    let caps = re.captures(&body)?;
    let limit: u32 = caps.get(1)?.as_str().parse().ok()?;

    tracing::info!("🔍 Probed model '{}': max_total_tokens = {}", model, limit);
    Some(limit)
}

/// Get context limit for a model, probing NIM if not cached.
async fn get_context_limit(
    cache: &ModelCache,
    client: &Client,
    config: &Config,
    model: &str,
    upstream_name: &str,
) -> u32 {
    // 1. Check cache
    {
        let cache_read = cache.read().await;
        if let Some(caps) = cache_read.get(model) {
            if caps.probed_at.elapsed().as_secs() < CACHE_TTL_SECS {
                return caps.max_total_tokens;
            }
        }
    }

    // 2. Get upstream base URL (without /v1/chat/completions)
    let upstream = config
        .upstreams
        .get(upstream_name)
        .or_else(|| config.upstreams.get("default"));

    let (base_url, api_key) = match upstream {
        Some(u) => (u.base_url.clone(), u.api_key.as_deref()),
        None => (config.base_url.clone(), config.api_key.as_deref()),
    };

    // 3. Probe NIM
    if let Some(limit) = probe_model_limit(client, &base_url, api_key, model).await {
        let mut cache_write = cache.write().await;
        cache_write.insert(
            model.to_string(),
            ModelCapabilities {
                max_total_tokens: limit,
                probed_at: std::time::Instant::now(),
            },
        );
        return limit;
    }

    tracing::warn!(
        "⚠️ Could not probe model '{}', using default {}",
        model,
        DEFAULT_CONTEXT_LIMIT
    );
    DEFAULT_CONTEXT_LIMIT
}

// ─── Concurrency Shield: Per-Model Semaphore (Doc1b) ───────────────
// Opción B: MAX_CONCURRENT_PER_MODEL and PERMIT_TIMEOUT_SECS are now
// read from Config (via .env) instead of hardcoded constants.
// Defaults: 5 concurrent, 180s timeout.

/// Shared collection of per-model semaphores.
pub type ModelSemaphores = Arc<AsyncRwLock<HashMap<String, Arc<Semaphore>>>>;
pub type CircuitBreaker = Arc<circuit_breaker::CircuitBreaker>;

/// Acquire a concurrency permit for a specific NIM model.
async fn acquire_model_permit(
    semaphores: &ModelSemaphores,
    model: &str,
    max_concurrent: usize,
    permit_timeout: u64,
) -> ProxyResult<tokio::sync::OwnedSemaphorePermit> {
    let sem = {
        let read = semaphores.read().await;
        if let Some(s) = read.get(model) {
            s.clone()
        } else {
            drop(read);
            let mut write = semaphores.write().await;
            write
                .entry(model.to_string())
                .or_insert_with(|| {
                    tracing::info!(
                        "🛡️ Created concurrency semaphore for '{}' ({} permits)",
                        model,
                        max_concurrent,
                    );
                    Arc::new(Semaphore::new(max_concurrent))
                })
                .clone()
        }
    };

    let available = sem.available_permits();
    if available == 0 {
        tracing::warn!(
            "⏳ Model '{}' at capacity (0/{} permits) — waiting up to {}s",
            model,
            max_concurrent,
            permit_timeout,
        );
    } else {
        tracing::debug!(
            "🎫 Acquiring permit for '{}' ({}/{} available)",
            model,
            available,
            max_concurrent,
        );
    }

    match tokio::time::timeout(
        Duration::from_secs(permit_timeout),
        sem.clone().acquire_owned(),
    )
    .await
    {
        Ok(Ok(permit)) => {
            tracing::debug!(
                "🎫 Permit acquired for '{}' ({}/{} remaining)",
                model,
                sem.available_permits(),
                max_concurrent,
            );
            Ok(permit)
        }
        Ok(Err(_)) => {
            tracing::error!("🛡️ Semaphore CLOSED for '{}' — this is a bug", model);
            Err(ProxyError::Internal(format!(
                "Semaphore closed for '{}'",
                model
            )))
        }
        Err(_) => {
            tracing::error!(
                "⏰ Permit TIMEOUT for '{}' (waited {}s, 0/{} available)",
                model,
                permit_timeout,
                max_concurrent,
            );
            Err(ProxyError::Overloaded(format!(
                "Model '{}' concurrency limit reached ({} slots busy for {}s)",
                model, max_concurrent, permit_timeout,
            )))
        }
    }
}

// ─── End Infrastructure ────────────────────────────────────────────

/// Resilient send for non-streaming: returns parsed OpenAI response.
/// Auto-retries on 429 (rate limit) with exponential backoff.
/// Auto-clamps max_tokens on 400 (too large) and retries.
async fn resilient_send(
    client: &Client,
    config: &Config,
    openai_req: &mut openai::OpenAIRequest,
    upstream_name: &str,
) -> ProxyResult<openai::OpenAIResponse> {
    let mut attempt: u32 = 0;

    loop {
        attempt += 1;
        let mut req_builder = client
            .post(config.get_upstream_url(upstream_name))
            // v0.12.0: Prompt caching header (Gap #1)
            .header("anthropic-beta", "prompt-caching-2024-06-01")
            .json(&*openai_req)
            .timeout(Duration::from_secs(900));

        if let Some(api_key) = &config.get_upstream_key(upstream_name) {
            req_builder = req_builder.header("Authorization", format!("Bearer {}", api_key));
        }

        let response = req_builder.send().await?;
        let status = response.status();

        if status.is_success() {
            let resp: openai::OpenAIResponse = response.json().await?;
            if attempt > 1 {
                tracing::info!("🔄 Request succeeded on attempt #{}", attempt);
            }
            return Ok(resp);
        }

        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());

        // === Smart Retry: 3-Layer Error Classification ===
        let upstream_err = parse_upstream_error(status.as_u16(), &error_text);
        tracing::debug!(
            "🔍 Parsed error: status={}, type={:?}, param={:?}, msg={}",
            upstream_err.status,
            upstream_err.error_type,
            upstream_err.param,
            &upstream_err.message[..upstream_err.message.len().min(100)]
        );

        let class = classify_error(&upstream_err);
        tracing::debug!("🧠 Classified: {:?} (status={})", class, status.as_u16());

        match class {
            ErrorClass::Retryable {
                base_delay_ms,
                max_retries,
                reason,
            } => {
                if attempt >= max_retries {
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
                        &upstream_err.message[..upstream_err.message.len().min(300)]
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
                        &upstream_err.message[..upstream_err.message.len().min(300)]
                    )));
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
                    &upstream_err.message[..upstream_err.message.len().min(500)]
                );
                // v6.1/v10.2: input_tokens overflow → try to extract safe max_tokens
                if reason.contains("input_tokens overflow") {
                    return Err(ProxyError::ContextOverflow(format!(
                        "Context window full: {}. Use /compact to reduce context.",
                        &upstream_err.message[..upstream_err.message.len().min(300)]
                    )));
                }
                return Err(ProxyError::Upstream(format!(
                    "Fatal {} ({}): {}",
                    status,
                    reason,
                    &upstream_err.message[..upstream_err.message.len().min(300)]
                )));
            }
        }
    }
}

/// Resilient send for streaming: returns raw reqwest::Response (not parsed).
/// Same retry logic as resilient_send but returns the response for streaming.
async fn resilient_send_raw(
    client: &Client,
    config: &Config,
    openai_req: &mut openai::OpenAIRequest,
    upstream_name: &str,
) -> ProxyResult<reqwest::Response> {
    let mut attempt: u32 = 0;

    loop {
        attempt += 1;
        let mut req_builder = client
            .post(config.get_upstream_url(upstream_name))
            // v0.12.0: Prompt caching header (Gap #1)
            .header("anthropic-beta", "prompt-caching-2024-06-01")
            .json(&*openai_req)
            .timeout(Duration::from_secs(900));

        if let Some(api_key) = &config.get_upstream_key(upstream_name) {
            req_builder = req_builder.header("Authorization", format!("Bearer {}", api_key));
        }

        let response = req_builder.send().await?;
        let status = response.status();

        if status.is_success() {
            if attempt > 1 {
                tracing::info!("🔄 Streaming request succeeded on attempt #{}", attempt);
            }
            return Ok(response);
        }

        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());

        // === Smart Retry: 3-Layer Error Classification [stream] ===
        let upstream_err = parse_upstream_error(status.as_u16(), &error_text);
        tracing::debug!(
            "🔍 [stream] Parsed error: status={}, type={:?}, param={:?}, msg={}",
            upstream_err.status,
            upstream_err.error_type,
            upstream_err.param,
            &upstream_err.message[..upstream_err.message.len().min(100)]
        );

        let class = classify_error(&upstream_err);
        tracing::debug!(
            "🧠 [stream] Classified: {:?} (status={})",
            class,
            status.as_u16()
        );

        match class {
            ErrorClass::Retryable {
                base_delay_ms,
                max_retries,
                reason,
            } => {
                if attempt >= max_retries {
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
                        &upstream_err.message[..upstream_err.message.len().min(300)]
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
                            &upstream_err.message[..upstream_err.message.len().min(300)]
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
                        &upstream_err.message[..upstream_err.message.len().min(300)]
                    )));
                }
                // v10.2: For input_tokens overflow, calculate exact safe max_tokens
                let new_max = if reason.contains("input_tokens overflow") {
                    if let Some(safe) = extract_safe_max_tokens_from_error(&upstream_err.message) {
                        // v0.11.0: Subtract safety margin to absorb NIM re-tokenization drift
                        // NIM adds ~257 tokens per retry (chat template expansion).
                        // Without margin: attempt1=63743, NIM says input=139010 (+257) → fail
                        // With margin:    attempt1=61695, NIM says input=139010 → still fits
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
                    &upstream_err.message[..upstream_err.message.len().min(500)]
                );
                // v6.1/v10.2: input_tokens overflow → 400 (CC won't retry)
                if reason.contains("input_tokens overflow") {
                    return Err(ProxyError::ContextOverflow(format!(
                        "Context window full: {}. Use /compact to reduce context.",
                        &upstream_err.message[..upstream_err.message.len().min(300)]
                    )));
                }
                return Err(ProxyError::Upstream(format!(
                    "Fatal {} ({}): {}",
                    status,
                    reason,
                    &upstream_err.message[..upstream_err.message.len().min(300)]
                )));
            }
        }
    }
}

pub async fn proxy_handler(
    Extension(shared_config): Extension<SharedConfig>,
    Extension(client): Extension<Client>,
    Extension(_circuit_breaker): Extension<CircuitBreaker>,
    Extension(model_cache): Extension<ModelCache>,
    Extension(model_semaphores): Extension<ModelSemaphores>,
    Extension(calibration): Extension<tokenizer::CalibrationFactors>,
    Json(req): Json<anthropic::AnthropicRequest>,
) -> ProxyResult<Response> {
    // v0.11.0 (CR-04): Recover from poisoned RwLock instead of panicking
    let config = Arc::new(
        shared_config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone(),
    );
    let is_streaming = req.stream.unwrap_or(false);

    tracing::info!(
        "Received request: model={} streaming={}",
        req.model,
        is_streaming
    );

    // v8.0: Log CC thinking/effort params for forensic investigation
    if let Some(thinking) = req.extra.get("thinking") {
        tracing::info!(
            "🧠 CC thinking config: {}",
            serde_json::to_string(thinking).unwrap_or_default()
        );
    }
    // Also log if budget_tokens is present anywhere in extra
    tracing::debug!(
        "📦 CC extra fields: {}",
        serde_json::to_string(&req.extra).unwrap_or_default()
    );

    if config.verbose {
        tracing::trace!(
            "Incoming Anthropic request: {}",
            serde_json::to_string_pretty(&req).unwrap_or_default()
        );
    }

    let (mut openai_req, upstream_name) = transform::anthropic_to_openai(req.clone(), &config)?;

    // === Pre-check: Dynamic context limit clamping (Doc1) ===
    let context_limit = get_context_limit(
        &model_cache,
        &client,
        &config,
        &openai_req.model,
        &upstream_name,
    )
    .await;

    // v10.2: Use tiktoken (cl100k_base) for accurate pre-check instead of crude JSON.len()/4
    let estimated_input = tokenizer::estimate_from_openai_request(&openai_req);
    let requested_output = openai_req.max_tokens.unwrap_or(64000);

    if estimated_input + requested_output > context_limit {
        // Use 256-token safety margin to account for tiktoken vs NIM tokenizer differences
        let safe_output = context_limit
            .saturating_sub(estimated_input)
            .saturating_sub(256)
            .clamp(1024, 64000);
        tracing::warn!(
            "⚠️ Pre-check: ~{}tok + {}tok > {}tok (model={}, tiktoken). Clamping → {}",
            estimated_input,
            requested_output,
            context_limit,
            openai_req.model,
            safe_output
        );
        openai_req.max_tokens = Some(safe_output);
    }

    if config.verbose {
        tracing::trace!(
            "Transformed OpenAI request: {}",
            serde_json::to_string_pretty(&openai_req).unwrap_or_default()
        );
    }

    if is_streaming {
        let original_model = req.model.clone();
        handle_streaming(
            config,
            client,
            openai_req,
            &upstream_name,
            &original_model,
            model_semaphores,
            calibration,
            estimated_input, // v10.3: pass pre-computed estimate to avoid double tiktoken
            context_limit,   // v0.11.0 (CR-08): for input_tokens scaling
        )
        .await
    } else {
        handle_non_streaming(
            config,
            client,
            openai_req,
            req,
            &upstream_name,
            model_semaphores,
        )
        .await
    }
}

async fn handle_non_streaming(
    config: Arc<Config>,
    client: Client,
    openai_req: openai::OpenAIRequest,
    original_req: anthropic::AnthropicRequest,
    upstream_name: &str,
    model_semaphores: ModelSemaphores,
) -> ProxyResult<Response> {
    // ╔═══════════════════════════════════════════╗
    // ║  Concurrency Shield: acquire model permit  ║
    // ╚═══════════════════════════════════════════╝
    let _permit = acquire_model_permit(
        &model_semaphores,
        &openai_req.model,
        config.max_concurrent_per_model,
        config.permit_timeout_secs,
    )
    .await?;

    let url = config.get_upstream_url(upstream_name);
    tracing::debug!(
        "Sending non-streaming request to {} (upstream: {})",
        url,
        upstream_name
    );
    tracing::debug!("Request model: {}", openai_req.model);

    // State for web_fetch interception loop
    let mut current_openai_req = openai_req;
    let mut current_messages = original_req.messages.clone();
    let mut fetch_count: u32 = 0;

    loop {
        // === Resilient send with auto-retry on 429/400 ===
        let openai_resp =
            resilient_send(&client, &config, &mut current_openai_req, upstream_name).await?;

        if config.verbose {
            tracing::trace!(
                "Received OpenAI response: {}",
                serde_json::to_string_pretty(&openai_resp).unwrap_or_default()
            );
        }

        let anthropic_resp = transform::openai_to_anthropic(openai_resp, &original_req.model)?;

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
                    extra: json!({}),
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
                    extra: json!({}),
                };

                // Append to messages and rebuild request
                current_messages.push(assistant_tool_use);
                current_messages.push(user_tool_result);

                let mut rebuilt_req = original_req.clone();
                rebuilt_req.messages = current_messages.clone();
                (current_openai_req, _) = transform::anthropic_to_openai(rebuilt_req, &config)?;

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

#[allow(clippy::too_many_arguments)]
async fn handle_streaming(
    config: Arc<Config>,
    client: Client,
    openai_req: openai::OpenAIRequest,
    upstream_name: &str,
    original_model: &str,
    model_semaphores: ModelSemaphores,
    calibration: tokenizer::CalibrationFactors,
    precomputed_estimate: u32, // v10.3: reuse pre-check estimate, avoid double tiktoken
    context_limit: u32, // v0.11.0 (CR-08): real model context window for input_tokens scaling
) -> ProxyResult<Response> {
    // ╔═══════════════════════════════════════════╗
    // ║  Concurrency Shield: acquire model permit  ║
    // ╚═══════════════════════════════════════════╝
    let permit = acquire_model_permit(
        &model_semaphores,
        &openai_req.model,
        config.max_concurrent_per_model,
        config.permit_timeout_secs,
    )
    .await?;

    let url = config.get_upstream_url(upstream_name);
    tracing::debug!(
        "Sending streaming request to {} (upstream: {})",
        url,
        upstream_name
    );
    tracing::debug!("Request model: {}", openai_req.model);

    // === Resilient send with auto-retry on 429/400 ===
    let mut mutable_req = openai_req;

    // v10.3: Reuse pre-computed tiktoken estimate from proxy_handler pre-check
    // (eliminates redundant ~50-300ms tiktoken call per request)
    let raw_estimate = precomputed_estimate;
    // v8.0: Apply calibration factor for this model (converges to ~98% after ~50 reqs)
    let nim_model_name = mutable_req.model.clone();
    let calibrated_estimate = calibration.apply(&nim_model_name, raw_estimate);
    tracing::debug!(
        "🎯 Token estimate: raw={}, calibrated={} (factor={:.4}, model={})",
        raw_estimate,
        calibrated_estimate,
        calibration.get(&nim_model_name),
        nim_model_name
    );

    let response = resilient_send_raw(&client, &config, &mut mutable_req, upstream_name).await?;

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
        context_limit, // v0.11.0 (CR-08)
    );

    let mut headers = HeaderMap::new();
    headers.insert(
        "Content-Type",
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert("Cache-Control", HeaderValue::from_static("no-cache"));
    headers.insert("Connection", HeaderValue::from_static("keep-alive"));

    Ok((headers, Body::from_stream(sse_stream)).into_response())
}

#[allow(clippy::too_many_arguments)]
fn create_sse_stream(
    stream: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    original_model: String,
    _permit: tokio::sync::OwnedSemaphorePermit,
    estimated_input_tokens: u32, // v8.0: calibrated tiktoken estimate for message_start
    raw_tiktoken_estimate: u32,  // v8.0: raw (uncalibrated) for calibration feedback
    nim_model_name: String,      // v8.0: NIM model name for calibration key
    calibration: tokenizer::CalibrationFactors, // v8.0: calibration factors
    context_limit: u32,          // v0.11.0 (CR-08): real model context for scaling
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
    async_stream::stream! {
        // v0.11.0 (CR-08): Scale input_tokens so CC auto-compact triggers correctly
        // for models with context < Claude's 200K (e.g., Kimi K2.5 = 131K)
        // v0.12.0: Configurable CC context window (Gap #5)
        let cc_context_window: u32 = std::env::var("CC_CONTEXT_WINDOW")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(200_000);
        tracing::debug!(
            "📐 Auto-compact scaling: cc_context_window={}K, model_context={}K",
            cc_context_window / 1000,
            context_limit / 1000
        );
        let scale_tokens = |real_tokens: u32| -> u32 {
            if context_limit > 0 && context_limit < cc_context_window {
                let scaled = (real_tokens as f64 * cc_context_window as f64
                              / context_limit as f64) as u32;
                tracing::debug!("📐 Scaling input_tokens for CC compact: {} → {} (real_ctx={}K, cc_ctx={}K)",
                    real_tokens, scaled, context_limit / 1000, cc_context_window / 1000);
                scaled
            } else {
                real_tokens // GLM5 (202K) ≥ Claude (200K) → no scaling needed
            }
        };
        // ╔═════════════════════════════════════════════════════════╗
        // ║  Concurrency Shield: _permit lives here until stream    ║
        // ║  ends → slot freed only on completion/disconnect.       ║
        // ╚═════════════════════════════════════════════════════════╝
        let _ = &_permit;  // prevent compiler from optimizing the move away

        let mut buffer = String::with_capacity(8192);
        let mut message_id = None;
        let mut current_model = None;
        let mut content_index = 0;
        let mut tool_call_id = None;
        let mut _tool_call_name: Option<String> = None;
        let mut tool_call_args = String::new();
        let mut has_sent_message_start = false;
        let mut current_block_type: Option<String> = None;
        // Phase 7: Track original model for message_start
        let original_model_owned = original_model;
        // Phase 5/11: Track if any tool calls were emitted
        let mut tool_calls_emitted = false;
        // WebFetch streaming interception state
        let mut is_intercepting_fetch = false;
        let mut _fetch_tool_name: Option<String> = None;
        let mut _fetch_tool_id_buf: Option<String> = None;
        let mut fetch_args_buffer = String::new();
        let mut suppressed_block_start = false;
        // v7.0: Initialize with tiktoken estimate — NIM real tokens override at [DONE]
        let mut accumulated_input_tokens: u32 = estimated_input_tokens;
        let mut accumulated_output_tokens: u32 = 0;
        // v6.1: Buffer stop_reason so message_delta is emitted at [DONE] with real token counts
        let mut saved_stop_reason: Option<String> = None;
        // v6.2: Track if reasoning stream was poisoned by </previous_reasoning> XML
        let mut reasoning_poisoned = false;

        tokio::pin!(stream);

        // v0.11.0: Stream loop with timeout (CR-01) — prevents indefinite hang if NIM stops sending
        let chunk_timeout = Duration::from_secs(CHUNK_TIMEOUT_SECS);
        loop {
            let chunk = match tokio::time::timeout(chunk_timeout, stream.next()).await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break, // Stream ended normally
                Err(_) => {
                    // v0.11.0 (CR-01): NIM stopped sending chunks — graceful shutdown
                    tracing::error!("⏰ Stream chunk timeout after {}s — NIM stopped sending", CHUNK_TIMEOUT_SECS);
                    // Emit graceful end_turn so CC doesn't hang
                    if has_sent_message_start {
                        if current_block_type.is_some() {
                            let stop_block = json!({"type": "content_block_stop", "index": content_index});
                            yield Ok(Bytes::from(format!("event: content_block_stop\ndata: {}\n\n",
                                serde_json::to_string(&stop_block).unwrap_or_default())));
                        }
                        let delta_event = json!({
                            "type": "message_delta",
                            "delta": { "stop_reason": "end_turn", "stop_sequence": serde_json::Value::Null },
                            "usage": { "input_tokens": accumulated_input_tokens, "output_tokens": accumulated_output_tokens }
                        });
                        yield Ok(Bytes::from(format!("event: message_delta\ndata: {}\n\n",
                            serde_json::to_string(&delta_event).unwrap_or_default())));
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


                    // v0.11.0 (CR-02): Buffer size guard — prevents OOM if NIM sends data without delimiters
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
                                    // v6.1: Emit deferred message_delta with real token counts
                                    if let Some(ref stop) = saved_stop_reason {
                                        let delta_event = json!({
                                            "type": "message_delta",
                                            "delta": {
                                                "stop_reason": stop,
                                                "stop_sequence": serde_json::Value::Null
                                            },
                                            // v0.11.0 (CR-08): Scale input_tokens for CC auto-compact
                                            "usage": {
                                                "input_tokens": scale_tokens(accumulated_input_tokens),
                                                "output_tokens": accumulated_output_tokens
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

                                    // v5.0: Accumulate usage from NIM (sent only in last chunk)
                                    if let Some(usage) = &chunk.usage {
                                        // v8.0: Feed back real NIM tokens to calibrator
                                        calibration.update(
                                            &nim_model_name,
                                            raw_tiktoken_estimate,
                                            usage.prompt_tokens,
                                        );
                                        let delta_pct = if estimated_input_tokens > 0 {
                                            ((usage.prompt_tokens as f64 - estimated_input_tokens as f64)
                                                / estimated_input_tokens as f64) * 100.0
                                        } else { 0.0 };
                                        tracing::info!(
                                            "📊 NIM real: in={}, out={} | estimate was: {} | delta: {:.1}%",
                                            usage.prompt_tokens, usage.completion_tokens,
                                            estimated_input_tokens, delta_pct
                                        );
                                        accumulated_input_tokens = usage.prompt_tokens;
                                        accumulated_output_tokens = usage.completion_tokens;
                                    }

                                    if let Some(choice) = chunk.choices.first() {
                                        if !has_sent_message_start {
                                            let event = anthropic::StreamEvent::MessageStart {
                                                message: anthropic::MessageStartData {
                                                    id: message_id.clone().unwrap_or_default(),
                                                    message_type: "message".to_string(),
                                                    role: "assistant".to_string(),
                                                    model: original_model_owned.clone(),  // Phase 7: use original ClaudeModelID
                                                    usage: anthropic::Usage {
                                                        // v7.0: Use tiktoken estimate (>0) instead of always-zero
                                                        // v0.11.0 (CR-08): Scale for CC auto-compact
                                                        input_tokens: scale_tokens(
                                                            chunk.usage.as_ref()
                                                                .map(|u| u.prompt_tokens)
                                                                .filter(|&t| t > 0)
                                                                .unwrap_or(estimated_input_tokens)
                                                        ),
                                                        output_tokens: 0,  // Per Anthropic spec: always 0 at start
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
                                            // v6.2: Skip if reasoning was already poisoned
                                            if reasoning_poisoned {
                                                // Discard — model is emitting tool call XML inside reasoning
                                                tracing::debug!("🧹 Discarding poisoned reasoning chunk");
                                            } else {
                                                // Check if this chunk contains the poison delimiter
                                                let emit_text = if reasoning.contains("<previous_reasoning") {
                            // FIX F1: Detect opening tag (without closing >)
                            reasoning_poisoned = true;
                            let pos = reasoning.find("<previous_reasoning").unwrap_or(0);
                            let clean = &reasoning[..pos];
                            tracing::info!(
                                "🧹 Reasoning sanitized: cut at <previous_reasoning> ({} chars discarded)",
                                reasoning.len() - pos
                            );
                            if clean.trim().is_empty() {
                                None
                            } else {
                                Some(clean.to_string())
                            }
                        } else if let Some(pos) = reasoning.find("</previous_reasoning>") {
                                                    reasoning_poisoned = true;
                                                    let clean = &reasoning[..pos];
                                                    tracing::info!("🧹 Reasoning sanitized: cut at </previous_reasoning> ({} chars discarded)",
                                                        reasoning.len() - pos);
                                                    if clean.trim().is_empty() { None } else { Some(clean.to_string()) }
                                                } else if reasoning.contains("<tool_call>") {
                                                    reasoning_poisoned = true;
                                                    let clean = reasoning.split("<tool_call>").next().unwrap_or("");
                                                    tracing::info!("🧹 Reasoning sanitized: cut at <tool_call>");
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
                                                                "signature": "" // v0.11.0 (CR-07): Anthropic spec requires signature field
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
                                                // v10.1: Sanitize content blocks too — GLM5 sometimes
                                                // emits reasoning via content instead of reasoning_content
                                                let sanitized_content = if reasoning_poisoned {
                                                    // Everything after poison → discard content too
                                                    tracing::debug!("🧹 Discarding poisoned content chunk ({} chars)", content.len());
                                                    None
                        } else if content.contains("<previous_reasoning") {
                            // FIX F1b: Detect opening tag in content
                            reasoning_poisoned = true;
                            let pos = content.find("<previous_reasoning").unwrap_or(0);
                            let clean = &content[..pos];
                            tracing::info!("🧹 Content sanitized: cut at <previous_reasoning>");
                            if clean.trim().is_empty() {
                                None
                            } else {
                                Some(clean.to_string())
                            }
                                                } else if let Some(pos) = content.find("</previous_reasoning>") {
                                                    reasoning_poisoned = true;
                                                    let clean = &content[..pos];
                                                    tracing::info!("🧹 Content sanitized: cut at </previous_reasoning> ({} chars discarded)",
                                                        content.len() - pos);
                                                    if clean.trim().is_empty() { None } else { Some(clean.to_string()) }
                                                } else if content.contains("<previous_reasoning>") {
                                                    // Strip opening tag but keep reasoning text
                                                    let cleaned = content.replace("<previous_reasoning>", "");
                                                    if cleaned.trim().is_empty() { None } else { Some(cleaned) }
                                                } else {
                                                    Some(content.to_string())
                                                };

                                                if let Some(clean_content) = sanitized_content {
                                                    if current_block_type.as_deref() != Some("text") {
                                                        if current_block_type.is_some() {
                                                            let event = json!({
                                                                "type": "content_block_stop",
                                                                "index": content_index
                                                            });
                                                            let sse_data = format!("event: content_block_stop\ndata: {}\n\n",
                                                                serde_json::to_string(&event).unwrap_or_default());
                                                            yield Ok(Bytes::from(sse_data));
                                                            content_index += 1;
                                                        }

                                                        // Start text block
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

                                                    // Send text delta
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

                                        // Handle tool calls
                                        if let Some(tool_calls) = &choice.delta.tool_calls {
                                            for tool_call in tool_calls {
                                                if let Some(id) = &tool_call.id {
                                                    // Start of new tool call
                                                    if current_block_type.is_some() && !is_intercepting_fetch {
                                                        let event = json!({
                                                            "type": "content_block_stop",
                                                            "index": content_index
                                                        });
                                                        let sse_data = format!("event: content_block_stop\ndata: {}\n\n",
                                                            serde_json::to_string(&event).unwrap_or_default());
                                                        yield Ok(Bytes::from(sse_data));
                                                        content_index += 1;
                                                    }

                                                    tool_call_id = Some(id.clone());
                                                    tool_call_args.clear();
                                                    tool_calls_emitted = true; // Phase 5: track for stop_reason
                                                }

                                                if let Some(function) = &tool_call.function {
                                                    if let Some(name) = &function.name {
                                                        _tool_call_name = Some(name.clone());

                                                        // Check if this is a web_fetch tool
                                                        if web_fetch::is_web_fetch_tool(name) {
                                                            is_intercepting_fetch = true;
                                                            _fetch_tool_name = Some(name.clone());
                                                            _fetch_tool_id_buf = tool_call_id.clone();
                                                            fetch_args_buffer.clear();
                                                            suppressed_block_start = true;
                                                            tracing::info!("[WebFetch/Stream] Intercepting tool_use: {}", name);
                                                        } else {
                                                            // Normal tool — emit as before
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
                                                            // Accumulate args for later fetch
                                                            fetch_args_buffer.push_str(args);
                                                        } else {
                                                            // Normal tool — emit delta
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

                                        // Handle finish reason
                                        if let Some(finish_reason) = &choice.finish_reason {
                                            // If we intercepted a web_fetch, execute it now
                                            if is_intercepting_fetch {
                                                // Use regex-based extraction for malformed JSON
                                                let fetch_url = web_fetch::extract_url_from_raw(&fetch_args_buffer)
                                                    .unwrap_or_else(|| {
                                                        tracing::warn!("[WebFetch/Stream] Could not extract URL from buffer: {}", &fetch_args_buffer[..fetch_args_buffer.len().min(200)]);
                                                        String::new()
                                                    });

                                                let fetch_result = if fetch_url.is_empty() || !fetch_url.starts_with("http") {
                                                    format!("Error: Could not extract valid URL from tool args: {}",
                                                        &fetch_args_buffer[..fetch_args_buffer.len().min(200)])
                                                } else {
                                                    tracing::info!("[WebFetch/Stream] Executing fetch for: {}", fetch_url);

                                                    // Create a client for the fetch
                                                    let fetch_client = reqwest::Client::builder()
                                                        .timeout(std::time::Duration::from_secs(15))
                                                        .build()
                                                        .unwrap_or_else(|_| reqwest::Client::new());

                                                    match fetch_client
                                                        .get(&fetch_url)
                                                        .header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
                                                        .header("Accept", "text/html,application/xhtml+xml,*/*")
                                                        .timeout(std::time::Duration::from_secs(15))
                                                        .send()
                                                        .await
                                                    {
                                                        Ok(resp) => {
                                                            match resp.text().await {
                                                                Ok(body) => {
                                                                    let text = web_fetch::strip_html_tags(&body);
                                                                    if text.len() > 200_000 {
                                                                        format!("{}\n\n[Content truncated at 200000 characters]", &text[..200_000])
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

                                                // Emit the fetched content as a text block
                                                if suppressed_block_start {
                                                    // Start a text block instead of tool_use
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

                                                // Emit fetched content as text_delta
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

                                            // Close current content block
                                            if current_block_type.is_some() {
                                                let event = json!({
                                                    "type": "content_block_stop",
                                                    "index": content_index
                                                });
                                                let sse_data = format!("event: content_block_stop\ndata: {}\n\n",
                                                    serde_json::to_string(&event).unwrap_or_default());
                                                yield Ok(Bytes::from(sse_data));
                                            }

                                            // v6.1: Save stop_reason — message_delta deferred to [DONE]
                                            // so it includes real token counts from NIM's usage chunk
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
                    // v0.11.0 (HI-05): Use Anthropic-native error type instead of "stream_error"
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

/// Search for web_fetch tool_use in an Anthropic response
fn find_web_fetch_in_response(
    resp: &anthropic::AnthropicResponse,
) -> Option<(String, String, serde_json::Value)> {
    for content in &resp.content {
        if let anthropic::ResponseContent::ToolUse {
            id, name, input, ..
        } = content
        {
            if web_fetch::is_web_fetch_tool(name) {
                return Some((id.clone(), name.clone(), input.clone()));
            }
        }
    }
    None
}
