//! Startup model health-check (Issue #69).
//!
//! Probes every configured upstream model with a minimal chat completion
//! request at startup.  Results are logged at INFO level; failures WARN
//! but never abort the process — the operator may intentionally run with
//! a partial fleet.

use crate::config::Config;
use reqwest::Client;
use std::collections::HashSet;
use std::time::{Duration, Instant};

/// Result of a single model health probe.
#[derive(Debug)]
pub struct ModelHealthResult {
    pub model_id: String,
    pub upstream_name: String,
    pub status: HealthStatus,
    pub latency_ms: u64,
}

/// Outcome of a health probe.
#[derive(Debug, PartialEq, Eq)]
pub enum HealthStatus {
    Healthy,
    Unhealthy(String),
    Timeout,
    Skipped(String),
}

/// Health-check probe timeout.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Run health checks against all configured model mappings.
///
/// Iterates `config.model_map`, deduplicates by (upstream_name, target_model),
/// and sends a minimal OpenAI-format chat completion to each.
/// Returns one [`ModelHealthResult`] per unique target; never panics or aborts.
pub async fn check_model_health(config: &Config, client: &Client) -> Vec<ModelHealthResult> {
    let mut results = Vec::new();
    let mut seen = HashSet::new();

    for (claude_id, route) in &config.model_map {
        let dedup_key = format!("{}:{}", route.upstream_name, route.target_model);
        if !seen.insert(dedup_key) {
            continue; // already probed this upstream:model pair
        }

        let api_key = config.get_upstream_key(&route.upstream_name);
        if api_key.is_none() {
            results.push(ModelHealthResult {
                model_id: format!("{} -> {}", claude_id, route.target_model),
                upstream_name: route.upstream_name.clone(),
                status: HealthStatus::Skipped("no API key configured".into()),
                latency_ms: 0,
            });
            continue;
        }

        let url = config.get_upstream_url(&route.upstream_name);
        let result = probe_model(
            client,
            &url,
            api_key.as_deref().unwrap_or_default(),
            &route.target_model,
            &format!("{} -> {}", claude_id, route.target_model),
            &route.upstream_name,
        )
        .await;
        results.push(result);
    }

    // If no model_map entries, probe the default upstream with a generic model
    if config.model_map.is_empty() {
        let url = config.get_upstream_url("default");
        let api_key = config.get_upstream_key("default");
        if let Some(ref key) = api_key {
            let result =
                probe_model(client, &url, key, "default", "default upstream", "default").await;
            results.push(result);
        } else {
            results.push(ModelHealthResult {
                model_id: "default upstream".into(),
                upstream_name: "default".into(),
                status: HealthStatus::Skipped("no API key configured".into()),
                latency_ms: 0,
            });
        }
    }

    results
}

/// Send a minimal chat completion probe to a single model.
async fn probe_model(
    client: &Client,
    url: &str,
    api_key: &str,
    upstream_model: &str,
    display_id: &str,
    upstream_name: &str,
) -> ModelHealthResult {
    let body = serde_json::json!({
        "model": upstream_model,
        "messages": [{"role": "user", "content": "ping"}],
        "max_tokens": 1,
        "stream": false
    });

    let start = Instant::now();

    let resp = client
        .post(url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .timeout(PROBE_TIMEOUT)
        .json(&body)
        .send()
        .await;

    let latency_ms = start.elapsed().as_millis() as u64;

    match resp {
        Ok(response) => {
            let status_code = response.status().as_u16();
            let health_status = classify_probe_response(status_code);
            ModelHealthResult {
                model_id: display_id.to_string(),
                upstream_name: upstream_name.to_string(),
                status: health_status,
                latency_ms,
            }
        }
        Err(e) => {
            let status = if e.is_timeout() {
                HealthStatus::Timeout
            } else if e.is_connect() {
                HealthStatus::Unhealthy("connection refused".into())
            } else {
                HealthStatus::Unhealthy(format!("request failed: {e}"))
            };
            ModelHealthResult {
                model_id: display_id.to_string(),
                upstream_name: upstream_name.to_string(),
                status,
                latency_ms,
            }
        }
    }
}

/// Classify the HTTP status code from a probe response.
fn classify_probe_response(status: u16) -> HealthStatus {
    match status {
        200..=299 => HealthStatus::Healthy,
        429 => HealthStatus::Healthy, // rate limited = model exists and responds
        401 | 403 => HealthStatus::Unhealthy("authentication failed".into()),
        404 => HealthStatus::Unhealthy("model not found".into()),
        500..=599 => HealthStatus::Unhealthy(format!("server error: {status}")),
        _ => HealthStatus::Unhealthy(format!("unexpected status: {status}")),
    }
}

/// Log a summary table of health-check results.
pub fn log_health_summary(results: &[ModelHealthResult]) {
    if results.is_empty() {
        tracing::info!("[HEALTH] No models configured for health check");
        return;
    }

    let healthy = results.iter().filter(|r| r.status == HealthStatus::Healthy).count();
    let unhealthy = results
        .iter()
        .filter(|r| matches!(r.status, HealthStatus::Unhealthy(_) | HealthStatus::Timeout))
        .count();
    let skipped = results.iter().filter(|r| matches!(r.status, HealthStatus::Skipped(_))).count();

    tracing::info!(
        "[HEALTH] Startup health check: {} healthy, {} unhealthy, {} skipped ({} total)",
        healthy,
        unhealthy,
        skipped,
        results.len()
    );

    for result in results {
        match &result.status {
            HealthStatus::Healthy => {
                tracing::info!(
                    "[HEALTH]   UP   {} [{}] ({}ms)",
                    result.model_id,
                    result.upstream_name,
                    result.latency_ms
                );
            }
            HealthStatus::Unhealthy(reason) => {
                tracing::warn!(
                    "[HEALTH]   DOWN {} [{}] - {} ({}ms)",
                    result.model_id,
                    result.upstream_name,
                    reason,
                    result.latency_ms
                );
            }
            HealthStatus::Timeout => {
                tracing::warn!(
                    "[HEALTH]   DOWN {} [{}] - timeout ({}ms)",
                    result.model_id,
                    result.upstream_name,
                    result.latency_ms
                );
            }
            HealthStatus::Skipped(reason) => {
                tracing::info!(
                    "[HEALTH]   SKIP {} [{}] - {}",
                    result.model_id,
                    result.upstream_name,
                    reason
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_200_is_healthy() {
        assert_eq!(classify_probe_response(200), HealthStatus::Healthy);
    }

    #[test]
    fn classify_429_is_healthy() {
        // Rate limited means the model exists and responds
        assert_eq!(classify_probe_response(429), HealthStatus::Healthy);
    }

    #[test]
    fn classify_401_is_auth_failure() {
        assert_eq!(
            classify_probe_response(401),
            HealthStatus::Unhealthy("authentication failed".into())
        );
    }

    #[test]
    fn classify_403_is_auth_failure() {
        assert_eq!(
            classify_probe_response(403),
            HealthStatus::Unhealthy("authentication failed".into())
        );
    }

    #[test]
    fn classify_404_is_not_found() {
        assert_eq!(classify_probe_response(404), HealthStatus::Unhealthy("model not found".into()));
    }

    #[test]
    fn classify_500_is_server_error() {
        assert_eq!(
            classify_probe_response(500),
            HealthStatus::Unhealthy("server error: 500".into())
        );
    }

    #[test]
    fn classify_502_is_server_error() {
        assert_eq!(
            classify_probe_response(502),
            HealthStatus::Unhealthy("server error: 502".into())
        );
    }

    #[test]
    fn classify_unexpected_status() {
        assert_eq!(
            classify_probe_response(418),
            HealthStatus::Unhealthy("unexpected status: 418".into())
        );
    }

    #[test]
    fn health_status_equality() {
        assert_eq!(HealthStatus::Healthy, HealthStatus::Healthy);
        assert_eq!(HealthStatus::Timeout, HealthStatus::Timeout);
        assert_ne!(HealthStatus::Healthy, HealthStatus::Timeout);
        assert_ne!(HealthStatus::Unhealthy("a".into()), HealthStatus::Unhealthy("b".into()));
    }

    #[test]
    fn log_summary_empty_does_not_panic() {
        // Just verifies no panic — the tracing output isn't captured here
        log_health_summary(&[]);
    }

    #[test]
    fn log_summary_mixed_results_does_not_panic() {
        let results = vec![
            ModelHealthResult {
                model_id: "m1".into(),
                upstream_name: "default".into(),
                status: HealthStatus::Healthy,
                latency_ms: 100,
            },
            ModelHealthResult {
                model_id: "m2".into(),
                upstream_name: "bigmodel".into(),
                status: HealthStatus::Unhealthy("auth failed".into()),
                latency_ms: 50,
            },
            ModelHealthResult {
                model_id: "m3".into(),
                upstream_name: "cf".into(),
                status: HealthStatus::Timeout,
                latency_ms: 5000,
            },
            ModelHealthResult {
                model_id: "m4".into(),
                upstream_name: "default".into(),
                status: HealthStatus::Skipped("no key".into()),
                latency_ms: 0,
            },
        ];
        log_health_summary(&results);
    }
}
