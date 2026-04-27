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
pub mod non_streaming;
pub mod overflow_tracker;
pub mod rate_limit;
pub mod retry;
pub mod streaming;

use axum::{response::Response, Extension, Json};
use metrics::{counter, histogram};
use reqwest::Client;

use crate::config::{Config, SharedConfig};
use crate::error::{ProxyError, ProxyResult};
use crate::models::anthropic;
use crate::tokenizer;
use crate::transform;

// Public re-exports for types used in main.rs
pub use concurrency::{CircuitBreaker, ModelSemaphores};
pub use discovery::{get_context_limit, ModelCache};

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

/// Returns the context overflow threshold percentage (default: 80%).
/// Configurable via CC_OVERFLOW_THRESHOLD_PCT env var (range: 50-95).
pub(crate) fn get_overflow_threshold_pct() -> u32 {
    std::env::var("CC_OVERFLOW_THRESHOLD_PCT")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&pct| (50..=95).contains(&pct))
        .unwrap_or(80)
}

/// Resolve the effective CC context window for a given model.
/// Priority order:
/// 1. CC_MODEL_CONTEXT_WINDOWS per-model mapping (highest priority)
/// 2. CLAUDE_CODE_AUTO_COMPACT_WINDOW (set by CC itself)
/// 3. CC_CONTEXT_WINDOW (manual override)
/// 4. 200_000 fallback (default for standard Claude models)
pub(crate) fn resolve_cc_context_window(model_id: &str, config: &Config) -> u32 {
    // 1. Per-model mapping (CC_MODEL_CONTEXT_WINDOWS env var)
    if let Some(&window) = config.cc_model_context_windows.get(model_id) {
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

pub async fn proxy_handler(
    Extension(shared_config): Extension<SharedConfig>,
    Extension(client): Extension<Client>,
    Extension(circuit_breaker): Extension<CircuitBreaker>,
    Extension(model_cache): Extension<ModelCache>,
    Extension(model_semaphores): Extension<ModelSemaphores>,
    Extension(calibration): Extension<tokenizer::CalibrationFactors>,
    Json(req): Json<anthropic::AnthropicRequest>,
) -> ProxyResult<Response> {
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

    let transform_result = transform::anthropic_to_openai(req.clone(), &config)?;
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
            context_limit,     // FIX 6: pass context_limit for token scaling
            cc_context_window, // Issue #28: resolved dynamically
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
    fn test_default_threshold_is_80() {
        let _guard = TEST_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::new(vec!["CC_OVERFLOW_THRESHOLD_PCT"]);
        assert_eq!(get_overflow_threshold_pct(), 80);
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
        assert_eq!(get_overflow_threshold_pct(), 80);
    }

    #[test]
    fn test_threshold_above_maximum_rejected() {
        let _guard = TEST_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::new(vec!["CC_OVERFLOW_THRESHOLD_PCT"]);
        std::env::set_var("CC_OVERFLOW_THRESHOLD_PCT", "99");
        assert_eq!(get_overflow_threshold_pct(), 80);
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
        }
    }

    #[test]
    fn test_default_fallback_200k() {
        let _guard = TEST_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::new(vec!["CLAUDE_CODE_AUTO_COMPACT_WINDOW", "CC_CONTEXT_WINDOW"]);
        let config = make_minimal_config();
        assert_eq!(resolve_cc_context_window("claude-sonnet-4-6", &config), 200_000);
    }

    #[test]
    fn test_cc_context_window_override() {
        let _guard = TEST_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::new(vec!["CLAUDE_CODE_AUTO_COMPACT_WINDOW", "CC_CONTEXT_WINDOW"]);
        std::env::set_var("CC_CONTEXT_WINDOW", "150000");
        let config = make_minimal_config();
        assert_eq!(resolve_cc_context_window("claude-sonnet-4-6", &config), 150_000);
    }

    #[test]
    fn test_claude_auto_compact_takes_priority() {
        let _guard = TEST_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::new(vec!["CLAUDE_CODE_AUTO_COMPACT_WINDOW", "CC_CONTEXT_WINDOW"]);
        std::env::set_var("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "100000");
        std::env::set_var("CC_CONTEXT_WINDOW", "150000");
        let config = make_minimal_config();
        // CLAUDE_CODE_AUTO_COMPACT_WINDOW should take priority over CC_CONTEXT_WINDOW
        assert_eq!(resolve_cc_context_window("claude-sonnet-4-6", &config), 100_000);
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
        assert_eq!(resolve_cc_context_window("claude-opus-4-6", &config), 1_000_000);
        // Unmapped model should fall through to CLAUDE_CODE_AUTO_COMPACT_WINDOW
        assert_eq!(resolve_cc_context_window("claude-sonnet-4-6", &config), 100_000);
    }

    #[test]
    fn test_zero_values_rejected() {
        let _guard = TEST_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::new(vec!["CLAUDE_CODE_AUTO_COMPACT_WINDOW", "CC_CONTEXT_WINDOW"]);
        std::env::set_var("CC_CONTEXT_WINDOW", "0");
        let config = make_minimal_config();
        // Zero should be filtered out, falling back to 200K
        assert_eq!(resolve_cc_context_window("claude-sonnet-4-6", &config), 200_000);
    }
}
