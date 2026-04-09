use crate::config::{Config, SharedConfig};
use crate::error::{ProxyError, ProxyResult};
use crate::models::{anthropic, openai};
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
const RETRY_BASE_MS: u64 = 1500; // Kept for reference, overridden per error class
const MIN_CLAMP_TOKENS: u32 = 4096;

// ─── Smart Retry Infrastructure (3-Layer Error Classification) ─────────

/// Parsed NIM/OpenAI error response with structured fields
#[derive(Debug, Default)]
struct UpstreamError {
    status: u16,
    message: String,
    error_type: Option<String>,  // NIM: "BadRequestError", etc.
    param: Option<String>,       // NIM: "input_tokens", etc.
    #[allow(dead_code)]
    code: Option<String>,        // NIM: "400", etc.
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
            err.message = error_obj.get("message")
                .and_then(|v| v.as_str())
                .unwrap_or(body)
                .to_string();
            err.error_type = error_obj.get("type")
                .and_then(|v| v.as_str())
                .map(String::from);
            err.param = error_obj.get("param")
                .and_then(|v| v.as_str())
                .map(String::from);
            err.code = error_obj.get("code")
                .and_then(|v| match v {
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
        message: json.get("detail")
            .or_else(|| json.get("message"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        error_type: json.get("type").and_then(|v| v.as_str()).map(String::from),
        param: None,
        code: None,
    })
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
    Fixable {
        reason: &'static str,
    },
    /// Fatal error — return immediately to CC with Anthropic-native error type
    Fatal {
        reason: &'static str,
    },
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

/// 3-layer error classification.
///
/// Layer 0: Structural — uses NIM's typed error fields (error.type, error.param)
/// Layer 1: Content-Aware — pattern matching on error message body
/// Layer 2: Status-Based — HTTP status code fallback
fn classify_error(upstream: &UpstreamError) -> ErrorClass {
    let lower = upstream.message.to_lowercase();

    // ╔══════════════════════════════════════════════════════════╗
    // ║  LAYER 0: Structural — NIM typed error fields            ║
    // ╚══════════════════════════════════════════════════════════╝
    if let Some(ref etype) = upstream.error_type {
        match etype.as_str() {
            "BadRequestError" => {
                if upstream.param.as_deref() == Some("input_tokens") {
                    return ErrorClass::Fixable {
                        reason: "NIM BadRequestError: input_tokens overflow (L0)",
                    };
                }
            }
            _ => {}
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
        401 => ErrorClass::Fatal { reason: "401 unauthorized (L2)" },
        402 => ErrorClass::Fatal { reason: "402 billing error (L2)" },
        403 => ErrorClass::Fatal { reason: "403 forbidden (L2)" },
        404 => ErrorClass::Fatal { reason: "404 not found (L2)" },
        405 => ErrorClass::Fatal { reason: "405 method not allowed (L2)" },
        413 => ErrorClass::Fixable { reason: "413 payload too large (L2)" },
        422 => ErrorClass::Fatal { reason: "422 unprocessable entity (L2)" },
        400..=499 => ErrorClass::Fatal {
            reason: "unknown 4xx client error (L2)",
        },
        500..=599 => ErrorClass::Retryable {
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

    let mut req_builder = client.post(&url)
        .header("Content-Type", "application/json")
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
    let upstream = config.upstreams.get(upstream_name)
        .or_else(|| config.upstreams.get("default"));

    let (base_url, api_key) = match upstream {
        Some(u) => (u.base_url.clone(), u.api_key.as_deref()),
        None => (config.base_url.clone(), config.api_key.as_deref()),
    };

    // 3. Probe NIM
    if let Some(limit) = probe_model_limit(client, &base_url, api_key, model).await {
        let mut cache_write = cache.write().await;
        cache_write.insert(model.to_string(), ModelCapabilities {
            max_total_tokens: limit,
            probed_at: std::time::Instant::now(),
        });
        return limit;
    }

    tracing::warn!("⚠️ Could not probe model '{}', using default {}", model, DEFAULT_CONTEXT_LIMIT);
    DEFAULT_CONTEXT_LIMIT
}

// ─── Concurrency Shield: Per-Model Semaphore (Doc1b) ───────────────

/// Maximum concurrent requests to ANY SINGLE NIM model.
/// Empirically verified: NIM has ~4-5 concurrent limit per model.
/// We use 5 to match NIM's capacity with 6 agents.
const MAX_CONCURRENT_PER_MODEL: usize = 5;

/// How long to wait for a semaphore permit before returning an error.
const PERMIT_TIMEOUT_SECS: u64 = 180;

/// Shared collection of per-model semaphores.
pub type ModelSemaphores = Arc<AsyncRwLock<HashMap<String, Arc<Semaphore>>>>;

/// Acquire a concurrency permit for a specific NIM model.
async fn acquire_model_permit(
    semaphores: &ModelSemaphores,
    model: &str,
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
                        model, MAX_CONCURRENT_PER_MODEL,
                    );
                    Arc::new(Semaphore::new(MAX_CONCURRENT_PER_MODEL))
                })
                .clone()
        }
    };

    let available = sem.available_permits();
    if available == 0 {
        tracing::warn!(
            "⏳ Model '{}' at capacity (0/{} permits) — waiting up to {}s",
            model, MAX_CONCURRENT_PER_MODEL, PERMIT_TIMEOUT_SECS,
        );
    } else {
        tracing::debug!(
            "🎫 Acquiring permit for '{}' ({}/{} available)",
            model, available, MAX_CONCURRENT_PER_MODEL,
        );
    }

    match tokio::time::timeout(
        Duration::from_secs(PERMIT_TIMEOUT_SECS),
        sem.clone().acquire_owned(),
    ).await {
        Ok(Ok(permit)) => {
            tracing::debug!(
                "🎫 Permit acquired for '{}' ({}/{} remaining)",
                model, sem.available_permits(), MAX_CONCURRENT_PER_MODEL,
            );
            Ok(permit)
        }
        Ok(Err(_)) => {
            tracing::error!("🛡️ Semaphore CLOSED for '{}' — this is a bug", model);
            Err(ProxyError::Internal(format!("Semaphore closed for '{}'", model)))
        }
        Err(_) => {
            tracing::error!(
                "⏰ Permit TIMEOUT for '{}' (waited {}s, 0/{} available)",
                model, PERMIT_TIMEOUT_SECS, MAX_CONCURRENT_PER_MODEL,
            );
            Err(ProxyError::Overloaded(format!(
                "Model '{}' concurrency limit reached ({} slots busy for {}s)",
                model, PERMIT_TIMEOUT_SECS, MAX_CONCURRENT_PER_MODEL,
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
            .post(&config.get_upstream_url(upstream_name))
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
            upstream_err.status, upstream_err.error_type, upstream_err.param,
            &upstream_err.message[..upstream_err.message.len().min(100)]
        );

        let class = classify_error(&upstream_err);
        tracing::debug!("🧠 Classified: {:?} (status={})", class, status.as_u16());

        match class {
            ErrorClass::Retryable { base_delay_ms, max_retries, reason } => {
                if attempt >= max_retries {
                    tracing::error!(
                        "⛔ {} [{}]: exhausted {} retries — giving up",
                        status.as_u16(), reason, max_retries
                    );
                    return Err(ProxyError::Upstream(format!(
                        "{} after {} retries ({}): {}",
                        status, max_retries, reason,
                        &upstream_err.message[..upstream_err.message.len().min(300)]
                    )));
                }
                let delay = delay_with_jitter(base_delay_ms, attempt);
                tracing::warn!(
                    "🔄 {} [{}] (attempt {}/{}) — retrying in {}ms",
                    status.as_u16(), reason, attempt, max_retries, delay
                );
                tokio::time::sleep(Duration::from_millis(delay)).await;
                continue;
            }
            ErrorClass::Fixable { reason } => {
                if attempt >= MAX_RETRIES {
                    tracing::error!(
                        "⛔ Fixable [{}]: exhausted {} retries — giving up",
                        reason, MAX_RETRIES
                    );
                    return Err(ProxyError::Upstream(format!(
                        "Fixable error after {} retries ({}): {}",
                        MAX_RETRIES, reason,
                        &upstream_err.message[..upstream_err.message.len().min(300)]
                    )));
                }
                let current = openai_req.max_tokens.unwrap_or(64000);
                let new_max = (current / 2).max(MIN_CLAMP_TOKENS);
                tracing::warn!(
                    "🔧 {} [{}] (attempt {}/{}): clamping max_tokens {} → {}",
                    status.as_u16(), reason, attempt, MAX_RETRIES, current, new_max
                );
                openai_req.max_tokens = Some(new_max);
                continue;
            }
            ErrorClass::Fatal { reason } => {
                tracing::error!(
                    "💀 {} [{}]: {}",
                    status.as_u16(), reason,
                    &upstream_err.message[..upstream_err.message.len().min(500)]
                );
                return Err(ProxyError::Upstream(format!(
                    "Fatal {} ({}): {}",
                    status, reason, &upstream_err.message[..upstream_err.message.len().min(300)]
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
            .post(&config.get_upstream_url(upstream_name))
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
            upstream_err.status, upstream_err.error_type, upstream_err.param,
            &upstream_err.message[..upstream_err.message.len().min(100)]
        );

        let class = classify_error(&upstream_err);
        tracing::debug!("🧠 [stream] Classified: {:?} (status={})", class, status.as_u16());

        match class {
            ErrorClass::Retryable { base_delay_ms, max_retries, reason } => {
                if attempt >= max_retries {
                    tracing::error!(
                        "⛔ [stream] {} [{}]: exhausted {} retries — giving up",
                        status.as_u16(), reason, max_retries
                    );
                    return Err(ProxyError::Upstream(format!(
                        "{} after {} retries ({}): {}",
                        status, max_retries, reason,
                        &upstream_err.message[..upstream_err.message.len().min(300)]
                    )));
                }
                let delay = delay_with_jitter(base_delay_ms, attempt);
                tracing::warn!(
                    "🔄 [stream] {} [{}] (attempt {}/{}) — retrying in {}ms",
                    status.as_u16(), reason, attempt, max_retries, delay
                );
                tokio::time::sleep(Duration::from_millis(delay)).await;
                continue;
            }
            ErrorClass::Fixable { reason } => {
                if attempt >= MAX_RETRIES {
                    tracing::error!(
                        "⛔ [stream] Fixable [{}]: exhausted {} retries — giving up",
                        reason, MAX_RETRIES
                    );
                    return Err(ProxyError::Upstream(format!(
                        "Fixable error after {} retries ({}): {}",
                        MAX_RETRIES, reason,
                        &upstream_err.message[..upstream_err.message.len().min(300)]
                    )));
                }
                let current = openai_req.max_tokens.unwrap_or(64000);
                let new_max = (current / 2).max(MIN_CLAMP_TOKENS);
                tracing::warn!(
                    "🔧 [stream] {} [{}] (attempt {}/{}): clamping max_tokens {} → {}",
                    status.as_u16(), reason, attempt, MAX_RETRIES, current, new_max
                );
                openai_req.max_tokens = Some(new_max);
                continue;
            }
            ErrorClass::Fatal { reason } => {
                tracing::error!(
                    "💀 [stream] {} [{}]: {}",
                    status.as_u16(), reason,
                    &upstream_err.message[..upstream_err.message.len().min(500)]
                );
                return Err(ProxyError::Upstream(format!(
                    "Fatal {} ({}): {}",
                    status, reason, &upstream_err.message[..upstream_err.message.len().min(300)]
                )));
            }
        }
    }
}

pub async fn proxy_handler(
    Extension(shared_config): Extension<SharedConfig>,
    Extension(client): Extension<Client>,
    Extension(model_cache): Extension<ModelCache>,
    Extension(model_semaphores): Extension<ModelSemaphores>,
    Json(req): Json<anthropic::AnthropicRequest>,
) -> ProxyResult<Response> {
    // Read config from RwLock and clone for this request
    // This ensures we don't hold the lock across async boundaries
    let config = Arc::new(shared_config.read().unwrap().clone());
    let is_streaming = req.stream.unwrap_or(false);

    tracing::info!(
        "Received request: model={} streaming={}",
        req.model,
        is_streaming
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
        &model_cache, &client, &config, &openai_req.model, &upstream_name,
    ).await;

    let estimated_input = serde_json::to_string(&openai_req.messages)
        .map(|s| s.len() / 4)
        .unwrap_or(0) as u32;
    let requested_output = openai_req.max_tokens.unwrap_or(64000);

    if estimated_input + requested_output > context_limit {
        let safe_output = context_limit
            .saturating_sub(estimated_input)
            .min(64000)
            .max(1024);
        tracing::warn!(
            "⚠️ Pre-check: ~{}tok + {}tok > {}tok (model={}, probed). Clamping → {}",
            estimated_input, requested_output, context_limit,
            openai_req.model, safe_output
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
        handle_streaming(config, client, openai_req, &upstream_name, &original_model, model_semaphores).await
    } else {
        handle_non_streaming(config, client, openai_req, req, &upstream_name, model_semaphores).await
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
    let _permit = acquire_model_permit(&model_semaphores, &openai_req.model).await?;

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

async fn handle_streaming(
    config: Arc<Config>,
    client: Client,
    openai_req: openai::OpenAIRequest,
    upstream_name: &str,
    original_model: &str,
    model_semaphores: ModelSemaphores,
) -> ProxyResult<Response> {
    // ╔═══════════════════════════════════════════╗
    // ║  Concurrency Shield: acquire model permit  ║
    // ╚═══════════════════════════════════════════╝
    let permit = acquire_model_permit(&model_semaphores, &openai_req.model).await?;

    let url = config.get_upstream_url(upstream_name);
    tracing::debug!(
        "Sending streaming request to {} (upstream: {})",
        url,
        upstream_name
    );
    tracing::debug!("Request model: {}", openai_req.model);

    // === Resilient send with auto-retry on 429/400 ===
    let mut mutable_req = openai_req;
    let response = resilient_send_raw(&client, &config, &mut mutable_req, upstream_name).await?;

    let stream = response.bytes_stream();
    let original_model_owned = original_model.to_string();
    let sse_stream = create_sse_stream(stream, original_model_owned, permit);

    let mut headers = HeaderMap::new();
    headers.insert(
        "Content-Type",
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert("Cache-Control", HeaderValue::from_static("no-cache"));
    headers.insert("Connection", HeaderValue::from_static("keep-alive"));

    Ok((headers, Body::from_stream(sse_stream)).into_response())
}

fn create_sse_stream(
    stream: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    original_model: String,
    _permit: tokio::sync::OwnedSemaphorePermit,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
    async_stream::stream! {
        // ╔═════════════════════════════════════════════════════════╗
        // ║  Concurrency Shield: _permit lives here until stream    ║
        // ║  ends → slot freed only on completion/disconnect.       ║
        // ╚═════════════════════════════════════════════════════════╝
        let _ = &_permit;  // prevent compiler from optimizing the move away

        let mut buffer = String::new();
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
        // v5.0: Fix 0-tokens — accumulate usage from NIM streaming chunks
        let mut accumulated_input_tokens: u32 = 0;
        let mut accumulated_output_tokens: u32 = 0;

        tokio::pin!(stream);

        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    let text = String::from_utf8_lossy(&bytes);
                    buffer.push_str(&text);

                    while let Some(pos) = buffer.find("\n\n") {
                        let line = buffer[..pos].to_string();
                        buffer = buffer[pos + 2..].to_string();

                        if line.trim().is_empty() {
                            continue;
                        }

                        for l in line.lines() {
                            if let Some(data) = l.strip_prefix("data: ") {
                                if data.trim() == "[DONE]" {
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
                                                        input_tokens: chunk.usage.as_ref().map(|u| u.prompt_tokens).unwrap_or(0),
                                                        output_tokens: chunk.usage.as_ref().map(|u| u.completion_tokens).unwrap_or(0),
                                                    },
                                                },
                                            };
                                            let sse_data = format!("event: message_start\ndata: {}\n\n",
                                                serde_json::to_string(&event).unwrap_or_default());
                                            yield Ok(Bytes::from(sse_data));
                                            has_sent_message_start = true;
                                        }

                                        let reasoning_val = choice.delta.reasoning_content.as_ref()
                                            .or(choice.delta.reasoning.as_ref());
                                        if let Some(reasoning) = reasoning_val {
                                            if current_block_type.is_none() {
                                                let event = json!({
                                                    "type": "content_block_start",
                                                    "index": content_index,
                                                    "content_block": {
                                                        "type": "thinking",
                                                        "thinking": ""
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
                                                    "thinking": reasoning
                                                }
                                            });
                                            let sse_data = format!("event: content_block_delta\ndata: {}\n\n",
                                                serde_json::to_string(&event).unwrap_or_default());
                                            yield Ok(Bytes::from(sse_data));
                                        }

                                        if let Some(content) = &choice.delta.content {
                                            if !content.is_empty() {
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
                                                        "text": content
                                                    }
                                                });
                                                let sse_data = format!("event: content_block_delta\ndata: {}\n\n",
                                                    serde_json::to_string(&event).unwrap_or_default());
                                                yield Ok(Bytes::from(sse_data));
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

                                            // Send message_delta with stop_reason
                                            let stop_reason = transform::map_stop_reason(Some(finish_reason), tool_calls_emitted); // Phase 5: tool detection
                                            let event = json!({
                                                "type": "message_delta",
                                                "delta": {
                                                    "stop_reason": stop_reason,
                                                    "stop_sequence": serde_json::Value::Null
                                                },
                                                "usage": {
                                                    "input_tokens": accumulated_input_tokens,
                                                    "output_tokens": accumulated_output_tokens
                                                }
                                            });
                                            let sse_data = format!("event: message_delta\ndata: {}\n\n",
                                                serde_json::to_string(&event).unwrap_or_default());
                                            yield Ok(Bytes::from(sse_data));
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
                            "type": "stream_error",
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
