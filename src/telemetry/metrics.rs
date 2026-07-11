//! Extended Prometheus metrics for telemetry.
//!
//! Adds client_type labels to existing metrics infrastructure.

use crate::telemetry::fingerprint::ClientType;
use metrics::{counter, gauge, histogram};

/// Record a request classified by client type.
/// Increments `nexus_requests_by_client_type` counter with the appropriate label.
pub fn record_client_type_request(client_type: ClientType) {
    counter!(
        "nexus_requests_by_client_type",
        "client_type" => client_type.to_string()
    )
    .increment(1);
}

/// Update the gauge for unique users seen today.
/// Called once per day after daily aggregation runs.
pub fn record_unique_users(count: u64) {
    gauge!("nexus_unique_users_today").set(count as f64);
}

/// Issue #75/#76: Record a request against the actual upstream model (as opposed to the
/// `nexus_requests_total` counter, which labels by the original Claude-alias model).
/// Increments `nexus_upstream_requests_total`.
pub fn record_upstream_request(upstream_model: &str, upstream_name: &str, streaming: bool) {
    counter!(
        "nexus_upstream_requests_total",
        "upstream_model" => upstream_model.to_string(),
        "upstream_name" => upstream_name.to_string(),
        "streaming" => streaming.to_string(),
    )
    .increment(1);
}

/// Issue #76: Record upstream request duration (stream start to completion for streaming;
/// full handler duration for non-streaming). Records `nexus_upstream_duration_seconds`.
pub fn record_upstream_duration(upstream_model: &str, upstream_name: &str, duration_secs: f64) {
    histogram!(
        "nexus_upstream_duration_seconds",
        "upstream_model" => upstream_model.to_string(),
        "upstream_name" => upstream_name.to_string(),
    )
    .record(duration_secs);
}

/// Issue #76: Record an upstream error with its classification reason (see
/// `ErrorClass::reason()`). Increments `nexus_upstream_errors_total`.
pub fn record_upstream_error(upstream_model: &str, upstream_name: &str, error_class: &str) {
    counter!(
        "nexus_upstream_errors_total",
        "upstream_model" => upstream_model.to_string(),
        "upstream_name" => upstream_name.to_string(),
        "error_class" => error_class.to_string(),
    )
    .increment(1);
}

/// Issue #76: Record input/output token usage per upstream model. Increments
/// `nexus_upstream_tokens_total`, split by `direction` label. Zero-valued directions are
/// skipped so the series is only created when that direction actually occurred.
pub fn record_upstream_tokens(upstream_model: &str, input_tokens: u32, output_tokens: u32) {
    if input_tokens > 0 {
        counter!(
            "nexus_upstream_tokens_total",
            "upstream_model" => upstream_model.to_string(),
            "direction" => "input",
        )
        .increment(input_tokens as u64);
    }
    if output_tokens > 0 {
        counter!(
            "nexus_upstream_tokens_total",
            "upstream_model" => upstream_model.to_string(),
            "direction" => "output",
        )
        .increment(output_tokens as u64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_type_display_values() {
        assert_eq!(ClientType::ClaudeCode.to_string(), "claude_code");
        assert_eq!(ClientType::AnthropicSDK.to_string(), "sdk");
        assert_eq!(ClientType::CustomScript.to_string(), "script");
        assert_eq!(ClientType::Unknown.to_string(), "unknown");
    }

    #[test]
    fn record_client_type_does_not_panic() {
        // Just verify it doesn't panic — metrics macros are safe to call
        record_client_type_request(ClientType::ClaudeCode);
        record_client_type_request(ClientType::Unknown);
    }

    #[test]
    fn record_unique_users_does_not_panic() {
        record_unique_users(42);
        record_unique_users(0);
    }

    #[test]
    fn test_record_upstream_request_does_not_panic() {
        record_upstream_request("meta/llama-3-70b-instruct", "default", true);
        record_upstream_request("meta/llama-3-70b-instruct", "default", false);
    }

    #[test]
    fn test_record_upstream_duration_does_not_panic() {
        record_upstream_duration("meta/llama-3-70b-instruct", "default", 1.234);
        record_upstream_duration("meta/llama-3-70b-instruct", "default", 0.0);
    }

    #[test]
    fn test_record_upstream_error_does_not_panic() {
        record_upstream_error("meta/llama-3-70b-instruct", "default", "429 rate limit (L2)");
    }

    #[test]
    fn test_record_upstream_tokens_does_not_panic() {
        record_upstream_tokens("meta/llama-3-70b-instruct", 100, 50);
    }

    #[test]
    fn test_record_upstream_tokens_skips_zero() {
        // Zero-valued directions must not panic and must not emit a series for that direction.
        record_upstream_tokens("meta/llama-3-70b-instruct", 0, 0);
    }
}
