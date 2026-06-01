//! Extended Prometheus metrics for telemetry.
//!
//! Adds client_type labels to existing metrics infrastructure.

use crate::telemetry::fingerprint::ClientType;
use metrics::{counter, gauge};

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
}
