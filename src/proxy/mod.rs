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
pub mod rate_limit;
pub mod retry;
pub mod streaming;

use axum::{response::Response, Extension, Json};
use metrics::{counter, histogram};
use reqwest::Client;

use crate::config::SharedConfig;
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
