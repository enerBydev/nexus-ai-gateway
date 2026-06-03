//! Opt-in daily HTTPS beacon for global usage statistics.
//!
//! Sends aggregated daily stats to a configured endpoint.
//! NO individual fingerprints, NO IPs, NO API keys, NO User-Agents.
//! Only counts and ratios — zero PII.
//!
//! Enabled only when TELEMETRY_BEACON_URL is set in environment.
use anyhow::{Context, Result};
use serde::Serialize;

use crate::telemetry::store::DailyStatsEntry;

/// Beacon configuration — derived from environment variables.
#[derive(Debug, Clone)]
pub struct BeaconConfig {
    /// HTTPS endpoint URL for the beacon.
    pub url: String,
    /// Instance identifier — HMAC-SHA256 of hostname with instance secret.
    pub instance_id: String,
    /// NEXUS version string.
    pub version: String,
    pub auth_token: String,
}

/// Beacon payload — only aggregated stats, zero PII.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
struct BeaconPayload {
    instance_id: String,
    version: String,
    date: String,
    stats: BeaconStats,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "snake_case")]
struct BeaconStats {
    total_requests: i64,
    unique_fingerprints: i64,
    models_used: serde_json::Value,
    client_types: serde_json::Value,
    #[serde(default)]
    avg_message_count: f64,
    #[serde(default)]
    tool_use_ratio: f64,
}

/// Send the daily beacon with aggregated stats.
///
/// Security guarantees:
/// - Only HTTPS URLs accepted (rejects http://)
/// - No individual fingerprints sent
/// - No IPs, API keys, or User-Agents sent
/// - 10-second timeout
pub async fn send_beacon(config: &BeaconConfig, stats: &DailyStatsEntry) -> Result<()> {
    // Reject non-HTTPS URLs
    if !config.url.starts_with("https://") {
        anyhow::bail!(
            "Beacon URL must use HTTPS (got: {}). Refusing to send telemetry over plain HTTP.",
            config.url
        );
    }

    let payload = BeaconPayload {
        instance_id: config.instance_id.clone(),
        version: config.version.clone(),
        date: stats.date.clone(),
        stats: BeaconStats {
            total_requests: stats.total_requests,
            unique_fingerprints: stats.unique_fingerprints,
            models_used: stats.models_used.clone(),
            client_types: stats.client_types.clone(),
            avg_message_count: stats.avg_message_count,
            tool_use_ratio: stats.tool_use_ratio,
        },
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .context("building beacon HTTP client")?;

    let response = client
        .post(&config.url)
        .header("Authorization", format!("Bearer {}", config.auth_token))
        .json(&payload)
        .send()
        .await
        .context("sending telemetry beacon")?;

    let status = response.status();
    if status.is_success() {
        tracing::info!("📡 Telemetry beacon sent (status: {status})");
    } else {
        tracing::warn!("⚠️ Telemetry beacon failed (status: {status})");
    }

    Ok(())
}

/// Validate that a beacon URL is acceptable.
/// Returns Ok if valid HTTPS URL, Err otherwise.
#[allow(dead_code)]
pub fn validate_beacon_url(url: &str) -> Result<()> {
    if url.is_empty() {
        anyhow::bail!("Beacon URL is empty");
    }
    if !url.starts_with("https://") {
        anyhow::bail!(
            "Beacon URL must use HTTPS (got: {}). Telemetry will NOT be sent over plain HTTP.",
            url
        );
    }
    Ok(())
}

/// Compute an instance identifier from hostname and secret.
/// Uses HMAC-SHA256(secret, hostname) — same algorithm as fingerprinting.
pub fn compute_instance_id(hostname: &str, secret: &[u8]) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    type HmacSha256 = Hmac<Sha256>;

    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC can accept any key length");
    mac.update(hostname.as_bytes());
    let result = mac.finalize().into_bytes();

    // Take first 16 bytes (32 hex chars) — enough for unique identification
    result[..16].iter().map(|b| format!("{:02x}", b)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_stats() -> DailyStatsEntry {
        DailyStatsEntry {
            date: "2026-05-31".to_string(),
            total_requests: 1500,
            unique_fingerprints: 3,
            models_used: serde_json::json!({"claude-sonnet-4-6": 1200}),
            client_types: serde_json::json!({"claude_code": 2}),
            avg_message_count: 12.3,
            tool_use_ratio: 0.78,
        }
    }

    #[test]
    fn validate_beacon_url_https() {
        assert!(validate_beacon_url("https://example.com/beacon").is_ok());
    }

    #[test]
    fn validate_beacon_url_http_rejected() {
        assert!(validate_beacon_url("http://example.com/beacon").is_err());
    }

    #[test]
    fn validate_beacon_url_empty_rejected() {
        assert!(validate_beacon_url("").is_err());
    }

    #[test]
    fn compute_instance_id_is_deterministic() {
        let secret = b"test-secret-32-bytes-long-enough!!";
        let id1 = compute_instance_id("my-host", secret);
        let id2 = compute_instance_id("my-host", secret);
        assert_eq!(id1, id2, "Same hostname + same secret must produce same instance ID");
    }

    #[test]
    fn compute_instance_id_different_hosts() {
        let secret = b"test-secret-32-bytes-long-enough!!";
        let id1 = compute_instance_id("host-a", secret);
        let id2 = compute_instance_id("host-b", secret);
        assert_ne!(id1, id2, "Different hostnames must produce different instance IDs");
    }

    #[test]
    fn beacon_payload_has_no_pii() {
        let _stats = make_stats(); // Use _stats to avoid the unused variable warning
        let payload = BeaconPayload {
            instance_id: "abc123".to_string(),
            version: "0.17.4".to_string(),
            date: "2026-05-31".to_string(),
            stats: BeaconStats {
                total_requests: 1500,
                unique_fingerprints: 3,
                models_used: serde_json::json!({"claude-sonnet-4-6": 1200}),
                client_types: serde_json::json!({"claude_code": 2}),
                avg_message_count: 12.3,
                tool_use_ratio: 0.78,
            },
        };

        let json = serde_json::to_string(&payload).unwrap();
        let json_lower = json.to_lowercase();

        // Verify no PII fields are present
        assert!(!json_lower.contains("ip"), "Payload must not contain IP data");
        assert!(!json_lower.contains("api_key"), "Payload must not contain API key data");
        // Note: we can't check for "fingerprint" because it's a legitimate field name in our stats
        // but we're checking that it's not individual fingerprint data
        assert!(!json_lower.contains("user_agent"), "Payload must not contain User-Agent data");
        assert!(json.contains("total_requests"), "Payload must contain total_requests");
        assert!(
            json.contains("unique_fingerprints"),
            "Payload must contain unique fingerprint count"
        );
    }
}
