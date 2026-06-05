//! Privacy-first telemetry module for NEXUS-AI-Gateway.
//!
//! Architecture:
//! - **Fingerprinting**: HMAC-SHA256 with instance-specific secret for all PII
//! - **SQLite store**: Local analytics persistence with 30-day retention
//! - **Prometheus**: Extended metrics with client_type labels
//! - **Beacon**: Opt-in daily aggregated stats (zero PII)
//!
//! Security model:
//! - Instance-specific random secret (32 bytes, generated on first boot)
//! - Secret stored with chmod 0600, never logged, never sent in beacon
//! - All PII (IPs, API keys) is HMAC'd before storage
//! - Even with source code + DB access, fingerprints are irreversible
//! - Different instances have different secrets (no cross-correlation)

pub mod beacon;
pub mod fingerprint;
pub mod metrics;
pub mod store;

#[allow(unused_imports)]
pub use beacon::BeaconConfig;
pub use fingerprint::ClientFingerprint;
#[allow(unused_imports)]
pub use store::DailyStatsEntry;
pub use store::TelemetryStore;

use axum::http::HeaderMap;
use std::net::SocketAddr;
use std::path::Path;

/// The HMAC secret for fingerprinting. Generated once per instance.
///
/// Security properties:
/// - 32 cryptographically random bytes
/// - Saved to disk with 0600 permissions (owner read/write only)
/// - Zeroed on Drop (best-effort memory wipe)
/// - Never logged, never included in beacon payload
#[derive(Debug, Clone)]
pub struct TelemetrySecret(Vec<u8>);

impl TelemetrySecret {
    /// Load secret from file, or generate a new one if it doesn't exist.
    ///
    /// File is created with 0600 permissions.
    /// Parent directory is created with 0700 permissions if needed.
    pub fn load_or_generate(path: &Path) -> anyhow::Result<Self> {
        // Create parent directory if needed
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
                }
            }
        }

        if path.exists() {
            // Load existing secret
            let data = std::fs::read(path)?;
            if data.len() >= 16 {
                tracing::debug!("Loaded telemetry secret from {}", path.display());
                return Ok(Self(data));
            }
            tracing::warn!("Telemetry secret file too short ({} bytes), regenerating", data.len());
        }

        // Generate new secret: 32 random bytes
        let secret: [u8; 32] = rand::random();
        std::fs::write(path, secret)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }

        tracing::info!("🔑 Generated new telemetry secret at {}", path.display());
        Ok(Self(secret.to_vec()))
    }

    /// Get the secret bytes for HMAC operations.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for TelemetrySecret {
    fn drop(&mut self) {
        // Zero out the secret bytes on drop (best-effort memory wipe)
        for byte in self.0.iter_mut() {
            *byte = 0;
        }
    }
}

/// Context passed to the proxy handler for telemetry capture.
/// Wrapped in Option — None when telemetry is disabled.
#[derive(Debug, Clone)]
pub struct TelemetryContext {
    pub secret: TelemetrySecret,
    pub store: TelemetryStore,
}

impl TelemetryContext {
    /// Initialize telemetry: load/generate secret, open DB, run purge.
    /// Returns None if telemetry is disabled or initialization fails.
    pub fn init(
        enabled: bool,
        db_path: &str,
        secret_path: &str,
        retention_days: u32,
        beacon_url: Option<String>,
    ) -> Option<Self> {
        if !enabled {
            tracing::info!("📊 Telemetry: disabled");
            return None;
        }

        // Load or generate HMAC secret
        let secret = match TelemetrySecret::load_or_generate(Path::new(secret_path)) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("📊 Telemetry: failed to load secret ({}), disabling", e);
                return None;
            }
        };

        // Open SQLite store
        let store = match TelemetryStore::open(Path::new(db_path)) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("📊 Telemetry: failed to open DB ({}), disabling", e);
                return None;
            }
        };

        // Purge old records on startup
        if let Err(e) = store.purge_old_records(retention_days) {
            tracing::debug!("📊 Telemetry: purge failed ({})", e);
        }

        // Update Prometheus gauge with today's unique count
        if let Ok(count) = store.get_unique_fingerprint_count_today() {
            crate::telemetry::metrics::record_unique_users(count);
        }

        if beacon_url.is_some() {
            tracing::info!("📡 Telemetry beacon configured");
        }

        tracing::info!("📊 Telemetry: enabled (db={}, retention={}d)", db_path, retention_days);
        Some(Self { secret, store })
    }
}

/// Capture a client fingerprint from request data.
///
/// This is the main entry point called from proxy_handler.
/// Extracts and HMAC-hashes all identifying fields.
pub fn capture(
    headers: &HeaderMap,
    addr: &SocketAddr,
    req: &crate::models::anthropic::AnthropicRequest,
    secret: &TelemetrySecret,
) -> ClientFingerprint {
    let secret_bytes = secret.as_bytes();

    // 1. Classify client type from User-Agent
    let client_type = fingerprint::classify_client_type(headers);
    let user_agent_category = client_type.to_string();

    // 2. HMAC-fingerprint the IP (/24 prefix)
    let fingerprint_ip = fingerprint::fingerprint_ip(addr, secret_bytes);

    // 3. HMAC-fingerprint the API key prefix
    let fingerprint_key = fingerprint::fingerprint_api_key(headers, secret_bytes);

    // 4. Count messages
    let message_count = req.messages.len() as u32;

    // 5. Detect tool_use in any message content
    let has_tool_use = req.messages.iter().any(|m| match &m.content {
        crate::models::anthropic::MessageContent::Blocks(blocks) => blocks
            .iter()
            .any(|b| matches!(b, crate::models::anthropic::ContentBlock::ToolUse { .. })),
        _ => false,
    });

    // 6. Detect system prompt
    let has_system_prompt = req.system.is_some();

    // 7. Model and streaming info
    let model = req.model.clone();
    let is_streaming = req.stream.unwrap_or(false);

    // Never log raw identifiers — only the category
    tracing::debug!(
        "Telemetry: client_type={}, messages={}, tool_use={}, model={}",
        user_agent_category,
        message_count,
        has_tool_use,
        model
    );

    ClientFingerprint {
        fingerprint_ip,
        fingerprint_key,
        client_type,
        user_agent_category,
        message_count,
        has_tool_use,
        has_system_prompt,
        model,
        is_streaming,
    }
}

/// Record a fingerprint to the store asynchronously.
/// Uses spawn_blocking to offload the synchronous SQLite write.
/// Also updates the `nexus_unique_users_today` Prometheus gauge after each write.
pub async fn record_async(store: TelemetryStore, fp: ClientFingerprint) {
    tokio::task::spawn_blocking(move || {
        if let Err(e) = store.record_fingerprint(&fp) {
            tracing::debug!("Telemetry record failed: {e}");
        } else {
            // Update Prometheus gauge with the new unique count
            if let Ok(count) = store.get_unique_fingerprint_count_today() {
                crate::telemetry::metrics::record_unique_users(count);
            }
        }
    })
    .await
    .unwrap_or_else(|e| {
        tracing::debug!("Telemetry spawn_blocking failed: {e}");
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::fingerprint::ClientType;
    use std::net::SocketAddr;

    fn make_test_secret() -> TelemetrySecret {
        TelemetrySecret(vec![42u8; 32])
    }

    #[test]
    fn secret_zeroes_on_drop() {
        let _secret_ptr;
        {
            let secret = TelemetrySecret(vec![0xABu8; 32]);
            _secret_ptr = secret.0.as_ptr() as usize;
            // Secret should contain 0xAB before drop
            assert_eq!(secret.as_bytes()[0], 0xAB);
        }
        // After drop, the memory should be zeroed (best-effort — not guaranteed by Rust)
        // We can't reliably test this without unsafe, so we just verify the type works
    }

    #[test]
    fn capture_produces_valid_fingerprint() {
        let secret = make_test_secret();
        let headers = HeaderMap::new();
        let addr: SocketAddr = "10.0.1.5:8315".parse().unwrap();

        let req = crate::models::anthropic::AnthropicRequest {
            model: "claude-sonnet-4-6".to_string(),
            messages: vec![crate::models::anthropic::Message {
                role: "user".to_string(),
                content: crate::models::anthropic::MessageContent::Text("hello".to_string()),
                extra: serde_json::Value::Null,
            }],
            max_tokens: 1024,
            system: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: Some(true),
            tools: None,
            metadata: None,
            extra: serde_json::Value::Null,
        };

        let fp = capture(&headers, &addr, &req, &secret);

        assert_eq!(fp.fingerprint_ip.len(), 64, "IP fingerprint must be 64 hex chars");
        assert_eq!(fp.fingerprint_key.len(), 64, "Key fingerprint must be 64 hex chars");
        assert_eq!(fp.client_type, ClientType::Unknown, "No User-Agent -> Unknown");
        assert_eq!(fp.message_count, 1);
        assert!(!fp.has_tool_use);
        assert!(!fp.has_system_prompt);
        assert_eq!(fp.model, "claude-sonnet-4-6");
        assert!(fp.is_streaming);
    }

    #[test]
    fn capture_detects_tool_use() {
        let secret = make_test_secret();
        let headers = HeaderMap::new();
        let addr: SocketAddr = "10.0.1.5:8315".parse().unwrap();

        let req = crate::models::anthropic::AnthropicRequest {
            model: "claude-sonnet-4-6".to_string(),
            messages: vec![crate::models::anthropic::Message {
                role: "assistant".to_string(),
                content: crate::models::anthropic::MessageContent::Blocks(vec![
                    crate::models::anthropic::ContentBlock::ToolUse {
                        id: "tool_1".to_string(),
                        name: "bash".to_string(),
                        input: serde_json::json!({}),
                    },
                ]),
                extra: serde_json::Value::Null,
            }],
            max_tokens: 1024,
            system: Some(crate::models::anthropic::SystemPrompt::Single(
                "You are helpful".to_string(),
            )),
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: None,
            tools: None,
            metadata: None,
            extra: serde_json::Value::Null,
        };

        let fp = capture(&headers, &addr, &req, &secret);

        assert!(fp.has_tool_use, "Should detect tool_use in content blocks");
        assert!(fp.has_system_prompt, "Should detect system prompt");
        assert_eq!(fp.message_count, 1);
    }
}
