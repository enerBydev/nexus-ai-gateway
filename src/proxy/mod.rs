//! Proxy module - NEXUS AI Gateway request handling
//!
//! This module handles the core proxy logic, including:
//! - Error type definitions and parsing
//! - Rate limit detection
//! - Error classification (3-layer system)
//! - Model capability discovery
//! - Concurrency control (per-model semaphores)
//! - Retry logic with exponential backoff
//! - Streaming and non-streaming request handling

pub mod classify;
pub mod concurrency;
pub mod discovery;
pub mod error_types;
pub mod headers;
pub mod non_streaming;
pub mod overflow_tracker;
pub mod rate_limit;
pub mod retry;
pub mod streaming;
pub mod token_scaling;

use axum::{http::HeaderMap, response::Response, Extension, Json};
use metrics::{counter, gauge, histogram};
use reqwest::Client;

use std::sync::atomic::{AtomicU64, Ordering};

use crate::config::{Config, SharedConfig};
use crate::error::{ProxyError, ProxyResult};
use crate::models::anthropic;
use crate::tokenizer;
use crate::transform;

// Public re-exports for types used in main.rs
pub use concurrency::{CircuitBreaker, ModelSemaphores};
pub use discovery::{get_context_limit, ModelCache};

/// Global counter of active proxy connections.
/// Incremented on request entry, decremented on exit (via ConnectionGuard).
pub static ACTIVE_CONNECTIONS: AtomicU64 = AtomicU64::new(0);

/// RAII guard that decrements ACTIVE_CONNECTIONS on Drop.
/// Ensures the counter is always decremented, even on panic or early return.
struct ConnectionGuard;

impl ConnectionGuard {
    fn new() -> Self {
        ACTIVE_CONNECTIONS.fetch_add(1, Ordering::Relaxed);
        let count = ACTIVE_CONNECTIONS.load(Ordering::Relaxed);
        tracing::debug!(" Connection opened (active: {})", count);
        gauge!("nexus_active_connections").set(count as f64);
        Self
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::Relaxed);
        let count = ACTIVE_CONNECTIONS.load(Ordering::Relaxed);
        tracing::debug!(" Connection closed (active: {})", count);
        gauge!("nexus_active_connections").set(count as f64);
    }
}

/// Validate semantic correctness of an Anthropic request before sending upstream.
/// Returns a ProxyError for invalid requests, preventing wasted API calls.
fn validate_request(req: &anthropic::AnthropicRequest) -> ProxyResult<()> {
    // Model must be non-empty
    if req.model.is_empty() {
        return Err(ProxyError::Transform("model field is required and cannot be empty".into()));
    }

    // Messages must be non-empty
    if req.messages.is_empty() {
        return Err(ProxyError::Transform(
            "messages array must contain at least one message".into(),
        ));
    }

    // max_tokens must be > 0
    if req.max_tokens == 0 {
        return Err(ProxyError::Transform("max_tokens must be greater than 0".into()));
    }

    // temperature must be in [0, 1] if specified
    if let Some(temp) = req.temperature {
        if !(0.0..=1.0).contains(&temp) {
            return Err(ProxyError::Transform(format!(
                "temperature must be between 0 and 1, got {}",
                temp
            )));
        }
    }

    // top_p must be in [0, 1] if specified
    if let Some(top_p) = req.top_p {
        if !(0.0..=1.0).contains(&top_p) {
            return Err(ProxyError::Transform(format!(
                "top_p must be between 0 and 1, got {}",
                top_p
            )));
        }
    }

    Ok(())
}

/// Returns the context overflow threshold percentage (default: 90%).
/// Aligned with CC's effective window after overhead subtraction (P5).
/// Configurable via CC_OVERFLOW_THRESHOLD_PCT env var (range: 50-95).
pub(crate) fn get_overflow_threshold_pct() -> u32 {
    std::env::var("CC_OVERFLOW_THRESHOLD_PCT")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&pct| (50..=95).contains(&pct))
        .unwrap_or(90)
}

/// Resolve the raw CC context window without system overhead.
/// Priority order:
/// 1. CC_MODEL_CONTEXT_WINDOWS per-model mapping (highest priority)
/// 2. CLAUDE_CODE_AUTO_COMPACT_WINDOW (set by CC itself)
/// 3. CC_CONTEXT_WINDOW (manual override)
/// 4. 200_000 fallback (default for standard Claude models)
pub(crate) fn resolve_raw_cc_context_window(model_id: &str, config: &Config) -> u32 {
    // 1. Per-model mapping (CC_MODEL_CONTEXT_WINDOWS env var)
    if let Some(&window) = config.cc_model_context_windows.get(model_id).filter(|&&w| w > 0) {
        tracing::debug!(
            "📐 CC context window from per-model mapping: {} → {}K",
            model_id,
            window / 1000
        );
        return window;
    }

    // 2. CLAUDE_CODE_AUTO_COMPACT_WINDOW (set by CC itself at runtime)
    if let Some(window) = std::env::var("CLAUDE_CODE_AUTO_COMPACT_WINDOW")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&w| w > 0)
    {
        tracing::debug!(
            "📐 CC context window from CLAUDE_CODE_AUTO_COMPACT_WINDOW: {}K",
            window / 1000
        );
        return window;
    }

    // 3. CC_CONTEXT_WINDOW (manual override, current behavior)
    if let Some(window) =
        std::env::var("CC_CONTEXT_WINDOW").ok().and_then(|v| v.parse().ok()).filter(|&w| w > 0)
    {
        tracing::debug!("📐 CC context window from CC_CONTEXT_WINDOW: {}K", window / 1000);
        return window;
    }

    // 4. Default: 200K (standard for Claude Sonnet/Opus/Haiku)
    tracing::debug!("📐 CC context window: default 200K");
    200_000
}

/// Resolve CC's effective context window, accounting for system overhead.
/// Matches Claude Code binary function Pd():
/// effective_window = raw_window - min(model_max_tokens, 20_000)
///
/// This ensures the proxy's overflow threshold aligns with CC's actual
/// auto-compact trigger (effective_window - 13_000).
pub(crate) fn resolve_cc_context_window(model_id: &str, config: &Config) -> u32 {
    let raw = resolve_raw_cc_context_window(model_id, config);
    token_scaling::resolve_effective_cc_context_window(raw, model_id)
}

#[allow(clippy::too_many_arguments)] // Axum extractors + HeaderMap for Issue #35
pub async fn proxy_handler(
    Extension(shared_config): Extension<SharedConfig>,
    Extension(client): Extension<Client>,
    Extension(circuit_breaker): Extension<CircuitBreaker>,
    Extension(model_cache): Extension<ModelCache>,
    Extension(model_semaphores): Extension<ModelSemaphores>,
    Extension(calibration): Extension<tokenizer::CalibrationFactors>,
    headers: HeaderMap, // Issue #35 F4: Extract client headers for forwarding
    Json(req): Json<anthropic::AnthropicRequest>,
) -> ProxyResult<Response> {
    // S3: Track active connections
    let _conn_guard = ConnectionGuard::new();

    // Phase 4.5: Capture request metrics
    let start = std::time::Instant::now();
    let is_streaming = req.stream.unwrap_or(false);
    let model_name = req.model.clone();

    // Phase 4.5: Record request counter
    counter!(
        "nexus_requests_total",
        "model" => model_name.clone(),
        "streaming" => is_streaming.to_string()
    )
    .increment(1);

    // Validate request before any upstream calls
    validate_request(&req)?;

    // v0.11.0 (CR-04): Recover from poisoned RwLock instead of panicking
    // v4.1: ArcSwap provides lock-free reads, no poisoning possible
    let config = shared_config.load_full();

    // Issue #35 F4: Extract client headers for Anthropic header forwarding
    let client_headers = crate::proxy::headers::ClientHeaders::from_headers(&headers);
    if client_headers.anthropic_beta.is_some() {
        tracing::debug!(
            "📥 Client anthropic-beta: {}",
            client_headers.anthropic_beta.as_deref().unwrap_or("")
        );
    }
    if client_headers.anthropic_version.is_some() {
        tracing::debug!(
            "📥 Client anthropic-version: {}",
            client_headers.anthropic_version.as_deref().unwrap_or("")
        );
    }

    tracing::info!("Received request: model={} streaming={}", req.model, is_streaming);

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

    // Issue #35 F9: Pre-resolve upstream_name for conditional chat_template_kwargs
    let (_, upstream_name_pre) = transform::resolve_model_and_upstream(&req.model, true, &config);
    let transform_result =
        transform::anthropic_to_openai(req.clone(), &config, &upstream_name_pre)?;
    let mut openai_req = transform_result.request;
    let upstream_name = transform_result.upstream_name;
    // PHASE 3.5: Cache markers available for cache integration (currently unused)
    let _cache_markers = transform_result.cache_markers;

    // === Pre-check: Dynamic context limit clamping (Doc1) ===
    let context_limit =
        get_context_limit(&model_cache, &client, &config, &openai_req.model, &upstream_name).await;

    // Issue #28: Resolve CC context window dynamically
    let cc_context_window = resolve_cc_context_window(&req.model, &config);

    // v10.2: Use tiktoken (cl100k_base) for accurate pre-check instead of crude JSON.len()/4
    let estimated_input = tokenizer::estimate_from_openai_request(&openai_req);
    let requested_output = openai_req.max_tokens.unwrap_or(64000);

    if estimated_input + requested_output > context_limit {
        // Use 256-token safety margin to account for tiktoken vs NIM tokenizer differences
        let safe_output =
            context_limit.saturating_sub(estimated_input).saturating_sub(256).clamp(1024, 64000);
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

    let result = if is_streaming {
        let original_model = req.model.clone();
        streaming::handle_streaming(
            config,
            client,
            openai_req,
            &upstream_name,
            &original_model,
            model_semaphores,
            calibration,
            estimated_input, // v10.3: pass pre-computed estimate to avoid double tiktoken
            context_limit,   // v0.11.0 (CR-08): for input_tokens scaling
            cc_context_window, // Issue #28: resolved dynamically
            &circuit_breaker,
            crate::SHUTDOWN_TOKEN.clone(), // NEW: pass cancellation token for graceful shutdown
            client_headers.clone(), // Issue #35 F5: forward client headers for Anthropic upstreams
        )
        .await
    } else {
        non_streaming::handle_non_streaming(
            config,
            client,
            openai_req,
            req,
            &upstream_name,
            model_semaphores,
            &circuit_breaker,
            context_limit,          // FIX 6: pass context_limit for token scaling
            cc_context_window,      // Issue #28: resolved dynamically
            client_headers.clone(), // Issue #35 F5: forward client headers for Anthropic upstreams
        )
        .await
    };

    // Phase 4.5: Record request duration histogram
    histogram!(
        "nexus_request_duration_seconds",
        "model" => model_name
    )
    .record(start.elapsed().as_secs_f64());

    result
}

#[cfg(test)]
/// Crate-level mutex for synchronizing tests that modify process environment variables.
/// All env-mutating tests MUST acquire this lock to prevent cross-test interference.
static TEST_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
/// RAII guard that clears specified env vars on creation and on drop.
/// Ensures test env vars never leak to other tests.
struct EnvGuard {
    keys: Vec<&'static str>,
}

#[cfg(test)]
impl EnvGuard {
    fn new(keys: Vec<&'static str>) -> Self {
        for k in &keys {
            std::env::remove_var(k);
        }
        Self { keys }
    }
}

#[cfg(test)]
impl Drop for EnvGuard {
    fn drop(&mut self) {
        for k in &self.keys {
            std::env::remove_var(k);
        }
    }
}

#[cfg(test)]
mod validation_tests {
    use super::*;
    use crate::models::anthropic;

    fn make_valid_request() -> anthropic::AnthropicRequest {
        anthropic::AnthropicRequest {
            model: "claude-sonnet-4-6".to_string(),
            messages: vec![anthropic::Message {
                role: "user".to_string(),
                content: anthropic::MessageContent::Text("Hello".to_string()),
                extra: serde_json::Value::Null,
            }],
            max_tokens: 1024,
            system: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: None,
            tools: None,
            metadata: None,
            extra: serde_json::Value::Null,
        }
    }

    #[test]
    fn test_valid_request_passes() {
        let req = make_valid_request();
        assert!(validate_request(&req).is_ok());
    }

    #[test]
    fn test_empty_model_rejected() {
        let mut req = make_valid_request();
        req.model = String::new();
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn test_empty_messages_rejected() {
        let mut req = make_valid_request();
        req.messages = vec![];
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn test_zero_max_tokens_rejected() {
        let mut req = make_valid_request();
        req.max_tokens = 0;
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn test_invalid_temperature_rejected() {
        let mut req = make_valid_request();
        req.temperature = Some(1.5);
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn test_negative_temperature_rejected() {
        let mut req = make_valid_request();
        req.temperature = Some(-0.5);
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn test_invalid_top_p_rejected() {
        let mut req = make_valid_request();
        req.top_p = Some(1.5);
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn test_boundary_temperature_accepted() {
        let mut req = make_valid_request();
        req.temperature = Some(0.0);
        assert!(validate_request(&req).is_ok());
        req.temperature = Some(1.0);
        assert!(validate_request(&req).is_ok());
    }
}

#[cfg(test)]
mod threshold_tests {
    use super::*;

    #[test]
    fn test_default_threshold_is_90() {
        let _guard = TEST_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::new(vec!["CC_OVERFLOW_THRESHOLD_PCT"]);
        assert_eq!(get_overflow_threshold_pct(), 90);
    }

    #[test]
    fn test_custom_threshold_valid() {
        let _guard = TEST_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::new(vec!["CC_OVERFLOW_THRESHOLD_PCT"]);
        std::env::set_var("CC_OVERFLOW_THRESHOLD_PCT", "75");
        assert_eq!(get_overflow_threshold_pct(), 75);
    }

    #[test]
    fn test_threshold_below_minimum_rejected() {
        let _guard = TEST_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::new(vec!["CC_OVERFLOW_THRESHOLD_PCT"]);
        std::env::set_var("CC_OVERFLOW_THRESHOLD_PCT", "40");
        assert_eq!(get_overflow_threshold_pct(), 90);
    }

    #[test]
    fn test_threshold_above_maximum_rejected() {
        let _guard = TEST_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::new(vec!["CC_OVERFLOW_THRESHOLD_PCT"]);
        std::env::set_var("CC_OVERFLOW_THRESHOLD_PCT", "99");
        assert_eq!(get_overflow_threshold_pct(), 90);
    }
}

#[cfg(test)]
mod context_window_tests {
    use super::*;

    fn make_minimal_config() -> Config {
        Config {
            port: 8315,
            base_url: "https://test.example.com".to_string(),
            api_key: Some("test-key".to_string()),
            reasoning_model: None,
            completion_model: None,
            debug: false,
            verbose: false,
            web_fetch_enabled: true,
            web_fetch_max_retries: 3,
            web_fetch_timeout_secs: 15,
            upstreams: Default::default(),
            model_map: Default::default(),
            max_concurrent_per_model: 5,
            permit_timeout_secs: 180,
            upstream_type: crate::config::UpstreamType::NIM,
            prompt_cache_enabled: false,
            prompt_cache_max_entries: 1000,
            prompt_cache_ttl_secs: 300,
            cb_enabled: false,
            cb_threshold: 10,
            cb_recovery_secs: 60,
            cc_model_context_windows: Default::default(),
            config_path: None,
        }
    }

    #[test]
    fn test_default_fallback_200k() {
        let _guard = TEST_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::new(vec!["CLAUDE_CODE_AUTO_COMPACT_WINDOW", "CC_CONTEXT_WINDOW"]);
        let config = make_minimal_config();
        assert_eq!(resolve_cc_context_window("claude-sonnet-4-6", &config), 180_000);
    }

    #[test]
    fn test_cc_context_window_override() {
        let _guard = TEST_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::new(vec!["CLAUDE_CODE_AUTO_COMPACT_WINDOW", "CC_CONTEXT_WINDOW"]);
        std::env::set_var("CC_CONTEXT_WINDOW", "150000");
        let config = make_minimal_config();
        assert_eq!(resolve_cc_context_window("claude-sonnet-4-6", &config), 130_000);
    }

    #[test]
    fn test_claude_auto_compact_takes_priority() {
        let _guard = TEST_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::new(vec!["CLAUDE_CODE_AUTO_COMPACT_WINDOW", "CC_CONTEXT_WINDOW"]);
        std::env::set_var("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "100000");
        std::env::set_var("CC_CONTEXT_WINDOW", "150000");
        let config = make_minimal_config();
        // CLAUDE_CODE_AUTO_COMPACT_WINDOW should take priority over CC_CONTEXT_WINDOW
        assert_eq!(resolve_cc_context_window("claude-sonnet-4-6", &config), 80_000);
    }

    #[test]
    fn test_per_model_mapping_highest_priority() {
        let _guard = TEST_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::new(vec!["CLAUDE_CODE_AUTO_COMPACT_WINDOW", "CC_CONTEXT_WINDOW"]);
        std::env::set_var("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "100000");
        std::env::set_var("CC_CONTEXT_WINDOW", "150000");
        let mut config = make_minimal_config();
        config.cc_model_context_windows.insert("claude-opus-4-6".to_string(), 1_000_000);
        // Per-model mapping should take priority over both env vars
        assert_eq!(resolve_cc_context_window("claude-opus-4-6", &config), 980_000);
        // Unmapped model should fall through to CLAUDE_CODE_AUTO_COMPACT_WINDOW
        assert_eq!(resolve_cc_context_window("claude-sonnet-4-6", &config), 80_000);
    }

    #[test]
    fn test_effective_window_subtracts_overhead() {
        let _guard = TEST_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::new(vec!["CLAUDE_CODE_AUTO_COMPACT_WINDOW", "CC_CONTEXT_WINDOW"]);
        let config = make_minimal_config();
        assert_eq!(resolve_cc_context_window("claude-sonnet-4-6", &config), 180_000);
    }

    #[test]
    fn test_effective_window_opus_4_6() {
        let _guard = TEST_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::new(vec!["CLAUDE_CODE_AUTO_COMPACT_WINDOW", "CC_CONTEXT_WINDOW"]);
        let config = make_minimal_config();
        assert_eq!(resolve_cc_context_window("claude-opus-4-6", &config), 180_000);
    }

    #[test]
    fn test_effective_window_old_model() {
        let _guard = TEST_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::new(vec!["CLAUDE_CODE_AUTO_COMPACT_WINDOW", "CC_CONTEXT_WINDOW"]);
        let config = make_minimal_config();
        // claude-3-sonnet: max_tokens=8192, reserved=min(8192,20000)=8192
        assert_eq!(resolve_cc_context_window("claude-3-sonnet", &config), 200_000 - 8_192);
    }
}

#[cfg(test)]
mod connection_counter_tests {
    use super::*;

    #[test]
    fn test_active_connections_increment_decrement() {
        ACTIVE_CONNECTIONS.store(0, Ordering::Relaxed);
        assert_eq!(ACTIVE_CONNECTIONS.load(Ordering::Relaxed), 0);

        ACTIVE_CONNECTIONS.fetch_add(1, Ordering::Relaxed);
        assert_eq!(ACTIVE_CONNECTIONS.load(Ordering::Relaxed), 1);

        ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::Relaxed);
        assert_eq!(ACTIVE_CONNECTIONS.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_connection_guard_raii() {
        ACTIVE_CONNECTIONS.store(0, Ordering::Relaxed);

        {
            let _guard = ConnectionGuard::new();
            assert_eq!(ACTIVE_CONNECTIONS.load(Ordering::Relaxed), 1);

            let _guard2 = ConnectionGuard::new();
            assert_eq!(ACTIVE_CONNECTIONS.load(Ordering::Relaxed), 2);
        }

        assert_eq!(ACTIVE_CONNECTIONS.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_connection_guard_manual_drop() {
        ACTIVE_CONNECTIONS.store(0, Ordering::Relaxed);

        let guard = ConnectionGuard::new();
        assert_eq!(ACTIVE_CONNECTIONS.load(Ordering::Relaxed), 1);

        // Explicitly drop the guard
        drop(guard);
        assert_eq!(ACTIVE_CONNECTIONS.load(Ordering::Relaxed), 0);
    }
}

// Phase 5: Graceful shutdown tests for S1-S11
#[cfg(test)]
mod drain_state_tests {
    use crate::IS_DRAINING;
    use std::sync::atomic::Ordering;

    #[test]
    fn test_is_draining_default_false() {
        // Reset to known state
        IS_DRAINING.store(false, Ordering::Relaxed);
        assert!(!IS_DRAINING.load(Ordering::Relaxed));
    }

    #[test]
    fn test_is_draining_set_true() {
        IS_DRAINING.store(true, Ordering::Relaxed);
        assert!(IS_DRAINING.load(Ordering::Relaxed));
        // Reset
        IS_DRAINING.store(false, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod shutdown_token_tests {
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn test_cancellation_token_cancelled() {
        let token = CancellationToken::new();
        assert!(!token.is_cancelled());
        token.cancel();
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn test_cancellation_token_clone_cancelled() {
        let token = CancellationToken::new();
        let clone = token.clone();
        token.cancel();
        assert!(clone.is_cancelled());
    }

    #[tokio::test]
    async fn test_cancellation_token_select() {
        let token = CancellationToken::new();
        let token_clone = token.clone();
        // Cancel after a short delay
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            token_clone.cancel();
        });
        // Should resolve when cancelled
        tokio::select! {
            _ = token.cancelled() => {
                // Success — token was cancelled
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                panic!("Token should have been cancelled");
            }
        }
    }
}

#[cfg(test)]
mod semaphore_early_release_tests {
    #[tokio::test]
    async fn test_permit_option_take_pattern() {
        // Verify the Option::take() pattern works for early release
        let semaphore = tokio::sync::Semaphore::new(1);
        let permit = semaphore.try_acquire().unwrap();
        let mut permit_opt: Option<tokio::sync::SemaphorePermit<'_>> = Some(permit);
        assert!(permit_opt.is_some());
        // Early release via take()
        if let Some(p) = permit_opt.take() {
            drop(p);
        }
        assert!(permit_opt.is_none());
        // After drop, a new permit can be acquired
        assert!(semaphore.try_acquire().is_ok());
    }
}

#[cfg(test)]
mod drain_timeout_tests {
    use super::*;

    fn get_drain_timeout_secs() -> u64 {
        std::env::var("DRAIN_TIMEOUT_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(30)
    }

    #[test]
    fn test_drain_timeout_default() {
        let _guard = TEST_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::new(vec!["DRAIN_TIMEOUT_SECS"]);
        // Default should be 30
        assert_eq!(get_drain_timeout_secs(), 30);
    }

    #[test]
    fn test_drain_timeout_custom() {
        let _guard = TEST_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::new(vec!["DRAIN_TIMEOUT_SECS"]);
        std::env::set_var("DRAIN_TIMEOUT_SECS", "60");
        assert_eq!(get_drain_timeout_secs(), 60);
    }

    #[test]
    fn test_drain_timeout_invalid_uses_default() {
        let _guard = TEST_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::new(vec!["DRAIN_TIMEOUT_SECS"]);
        std::env::set_var("DRAIN_TIMEOUT_SECS", "invalid");
        // Invalid value should fall back to default
        assert_eq!(get_drain_timeout_secs(), 30);
    }
}

#[cfg(test)]
mod service_file_tests {
    #[test]
    fn test_service_file_has_killmode_process() {
        let service_content = include_str!("../../scripts/nexus-ai-gateway.service");
        assert!(
            service_content.contains("KillMode=process"),
            "Service file must have KillMode=process"
        );
        assert!(
            service_content.contains("TimeoutStopSec=45"),
            "Service file must have TimeoutStopSec=45"
        );
        assert!(
            service_content.contains("SendSIGKILL=no"),
            "Service file must have SendSIGKILL=no"
        );
        assert!(
            !service_content.contains("ExecStartPre"),
            "Service file must NOT have ExecStartPre (can't be set when KillMode is process)"
        );
    }
}

#[cfg(test)]
mod logrotate_config_tests {
    #[test]
    fn test_logrotate_config_exists() {
        let logrotate_content = include_str!("../../scripts/logrotate-nexus.conf");
        assert!(logrotate_content.contains("/tmp/nexus-ai-gateway.log"));
        assert!(logrotate_content.contains("rotate 7"));
        assert!(logrotate_content.contains("copytruncate"));
        assert!(logrotate_content.contains("daily"));
        assert!(logrotate_content.contains("compress"));
    }
}

#[cfg(test)]
mod keepalive_interval_tests {
    use crate::proxy::streaming::KEEPALIVE_INTERVAL_SECS;

    #[test]
    fn test_keepalive_interval_is_30s() {
        // Verify the constant is accessible and correct
        assert_eq!(KEEPALIVE_INTERVAL_SECS, 30);
    }
}
