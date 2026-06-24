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
pub mod edit_metrics;
pub mod edit_rescue;
pub mod error_types;
pub mod headers;
pub mod non_streaming;
pub mod overflow_tracker;
pub mod rate_limit;
pub mod retry;
pub mod streaming;
pub mod token_scaling;

use axum::extract::ConnectInfo;
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
#[allow(dead_code)]
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

/// Issue #107/#119: minimum output budget that makes a request worth sending. When the
/// input leaves less than this inside the safe zone, the request is hopeless and is
/// rejected early instead of being sent to fail expensively (or return a degenerate
/// empty 200 that the non-streaming path can't decode).
pub(crate) const MIN_USEFUL_OUTPUT: u32 = 8192;

/// Outcome of the context pre-check.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PreCheck {
    /// Request fits inside the safe zone as-is.
    Ok,
    /// Clamp `max_tokens` to this value (leaves headroom below the context limit).
    Clamp { safe_output: u32, safe_zone: u32 },
    /// Reject — input alone leaves less than `MIN_USEFUL_OUTPUT` inside the safe zone.
    Reject { safe_zone: u32 },
}

/// Decide how to handle a request given its estimated input, requested output, the
/// upstream context limit, and the overflow threshold percentage (Issue #107/#119).
///
/// The safe zone is `context_limit * pct%`. We clamp the output so `input + output` stays
/// inside it (leaving headroom — NIM returns a degenerate empty 200 when the total reaches
/// ~98%+ of the limit, verified empirically), and reject outright when the input is so
/// large that no useful output budget remains.
pub(crate) fn context_precheck(
    estimated_input: u32,
    requested_output: u32,
    context_limit: u32,
    threshold_pct: u32,
) -> PreCheck {
    let safe_zone = ((context_limit as u64 * threshold_pct as u64) / 100) as u32;
    if estimated_input.saturating_add(MIN_USEFUL_OUTPUT) > safe_zone {
        return PreCheck::Reject { safe_zone };
    }
    if estimated_input.saturating_add(requested_output) > safe_zone {
        // max_fit >= MIN_USEFUL_OUTPUT here (otherwise we'd have rejected above), so the
        // min/max chain never inverts and never panics like `clamp` would.
        let safe_output =
            safe_zone.saturating_sub(estimated_input).min(requested_output).max(MIN_USEFUL_OUTPUT);
        return PreCheck::Clamp { safe_output, safe_zone };
    }
    PreCheck::Ok
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
            "[CALIB] CC context window from per-model mapping: {} -> {}K",
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
            "[CALIB] CC context window from CLAUDE_CODE_AUTO_COMPACT_WINDOW: {}K",
            window / 1000
        );
        return window;
    }

    // 3. CC_CONTEXT_WINDOW (manual override, current behavior)
    if let Some(window) =
        std::env::var("CC_CONTEXT_WINDOW").ok().and_then(|v| v.parse().ok()).filter(|&w| w > 0)
    {
        tracing::debug!("[CALIB] CC context window from CC_CONTEXT_WINDOW: {}K", window / 1000);
        return window;
    }

    // 4. Default: 200K (standard for Claude Sonnet/Opus/Haiku)
    tracing::debug!("[CALIB] CC context window: default 200K");
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
    Extension(telemetry_ctx): Extension<Option<crate::telemetry::TelemetryContext>>,
    headers: HeaderMap, // Issue #35 F4: Extract client headers for forwarding
    ConnectInfo(client_addr): ConnectInfo<std::net::SocketAddr>,
    Json(mut req): Json<anthropic::AnthropicRequest>,
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

    // v0.18.0: Telemetry capture (if enabled)
    if let Some(ref telemetry) = telemetry_ctx {
        let fp = crate::telemetry::capture(&headers, &client_addr, &req, &telemetry.secret);
        crate::telemetry::metrics::record_client_type_request(fp.client_type);
        let store = telemetry.store.clone();
        tokio::spawn(async move {
            crate::telemetry::record_async(store, fp).await;
        });
    }

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

    // Issue #88 P6 / #93: rescue Edit failures caused by CC Unicode normalization
    // before forwarding upstream — rewrites a unique fuzzy match on disk (guarded to
    // src/ within cwd) and turns the failing tool_result into success so the model
    // does not loop on an already-corrected file.
    crate::proxy::edit_rescue::rescue_request_edits(&mut req).await;

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

    // Issue #107/#119: leave headroom inside the context limit and reject hopeless
    // requests early (a near-full context makes NIM return a degenerate empty 200 that
    // the non-streaming path can't decode -> opaque 502).
    let threshold_pct = get_overflow_threshold_pct();
    match context_precheck(estimated_input, requested_output, context_limit, threshold_pct) {
        PreCheck::Reject { safe_zone } => {
            // Report the output budget actually left after the input, not the safe-zone total.
            let remaining_output = safe_zone.saturating_sub(estimated_input);
            tracing::warn!(
                "[WARN] Pre-check REJECT: ~{}tok input leaves {}tok < {}tok min output (safe zone {} = {}% of {}, model={}). Returning /compact error.",
                estimated_input, remaining_output, MIN_USEFUL_OUTPUT, safe_zone, threshold_pct, context_limit, openai_req.model
            );
            return Err(ProxyError::ContextOverflow(format!(
                "Context window nearly full: ~{} input tokens leave only {} tokens for output \
                 (fewer than the {} minimum; model limit {}, usable {}). Use /compact to reduce \
                 context, or switch to a larger-context model.",
                estimated_input, remaining_output, MIN_USEFUL_OUTPUT, context_limit, safe_zone
            )));
        }
        PreCheck::Clamp { safe_output, safe_zone } => {
            tracing::warn!(
                "[WARN] Pre-check: ~{}tok + {}tok > safe zone {} ({}% of {}, model={}). Clamping -> {} (headroom preserved)",
                estimated_input, requested_output, safe_zone, threshold_pct, context_limit, openai_req.model, safe_output
            );
            openai_req.max_tokens = Some(safe_output);
        }
        PreCheck::Ok => {}
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
/// Crate-level mutex for synchronizing tests that share global atomics
/// (ACTIVE_CONNECTIONS, IS_DRAINING). Prevents cross-test interference
/// when tests run in parallel (default cargo test behavior).
static GLOBAL_STATE_TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
            bind_addr: "127.0.0.1".to_string(),
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
            telemetry_enabled: false,
            telemetry_beacon_url: None,
            beacon_auth_token: None,
            telemetry_dir: "/tmp".to_string(),
            telemetry_db_path: "/tmp/nexus-telemetry.db".to_string(),
            telemetry_retention_days: 30,
            telemetry_secret_path: "/tmp/nexus-telemetry-secret".to_string(),
            config_path: None,
            telemetry_disabled_reason: None,
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
        let _guard = GLOBAL_STATE_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        ACTIVE_CONNECTIONS.store(0, Ordering::Relaxed);
        assert_eq!(ACTIVE_CONNECTIONS.load(Ordering::Relaxed), 0);

        ACTIVE_CONNECTIONS.fetch_add(1, Ordering::Relaxed);
        assert_eq!(ACTIVE_CONNECTIONS.load(Ordering::Relaxed), 1);

        ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::Relaxed);
        assert_eq!(ACTIVE_CONNECTIONS.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_connection_guard_raii() {
        let _guard = GLOBAL_STATE_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
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
        let _guard = GLOBAL_STATE_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
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
    use super::GLOBAL_STATE_TEST_MUTEX;
    use crate::IS_DRAINING;
    use std::sync::atomic::Ordering;

    #[test]
    fn test_is_draining_default_false() {
        let _guard = GLOBAL_STATE_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        // Reset to known state
        IS_DRAINING.store(false, Ordering::Relaxed);
        assert!(!IS_DRAINING.load(Ordering::Relaxed));
    }

    #[test]
    fn test_is_draining_set_true() {
        let _guard = GLOBAL_STATE_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
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

#[cfg(test)]
mod precheck_tests {
    use super::{context_precheck, PreCheck, MIN_USEFUL_OUTPUT};

    // glm-5.1 limit; safe_zone @90% = 182476.
    const LIMIT: u32 = 202752;

    #[test]
    fn rejects_when_input_leaves_no_useful_output() {
        // 190000 + 8192 > 182476 -> hopeless (Issue #107).
        assert_eq!(
            context_precheck(190_000, 64_000, LIMIT, 90),
            PreCheck::Reject { safe_zone: 182_476 }
        );
    }

    #[test]
    fn clamps_with_headroom_for_panthera_case() {
        // The exact failing case: 149379 input + 64000 requested. Clamp to the safe zone,
        // leaving ~10% headroom (was filling to 99.9% -> degenerate empty 200).
        assert_eq!(
            context_precheck(149_379, 64_000, LIMIT, 90),
            PreCheck::Clamp { safe_output: 33_097, safe_zone: 182_476 }
        );
    }

    #[test]
    fn clamp_caps_at_max_fit_not_requested() {
        // input 150000 + requested 40000 = 190000 > safe_zone -> clamp to 32476 (max fit).
        assert_eq!(
            context_precheck(150_000, 40_000, LIMIT, 90),
            PreCheck::Clamp { safe_output: 32_476, safe_zone: 182_476 }
        );
    }

    #[test]
    fn ok_when_request_fits_inside_safe_zone() {
        assert_eq!(context_precheck(10_000, 4_000, LIMIT, 90), PreCheck::Ok);
    }

    #[test]
    fn small_requested_below_min_useful_does_not_panic_and_is_ok() {
        // requested (4000) < MIN_USEFUL_OUTPUT but the total fits -> Ok, no clamp/panic.
        assert!(4_000 < MIN_USEFUL_OUTPUT);
        assert_eq!(context_precheck(170_000, 4_000, LIMIT, 90), PreCheck::Ok);
    }

    #[test]
    fn degenerate_empty_200_body_decodes_with_optional_usage() {
        // Issue #119: NIM's near-full-context degenerate 200. Must decode (usage Option)
        // so the empty-choices guard can turn it into a clear ContextOverflow error.
        let body = r#"{"id":"","choices":[],"created":0,"model":"","service_tier":null,"system_fingerprint":null,"object":"chat.completion","usage":null}"#;
        let resp: crate::models::openai::OpenAIResponse =
            serde_json::from_str(body).expect("degenerate body must decode with Option usage");
        assert!(resp.choices.is_empty(), "guard relies on empty choices");
        assert!(resp.usage.is_none(), "usage:null must decode to None");
    }
}
