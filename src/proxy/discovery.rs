use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;

use reqwest::Client;
use tokio::sync::RwLock as AsyncRwLock;

use crate::config::Config;

pub(crate) struct ProbeRegexes {
    pub(crate) max_total_tokens: regex::Regex,
}

pub(crate) fn probe_regexes() -> &'static ProbeRegexes {
    static RE: OnceLock<ProbeRegexes> = OnceLock::new();
    RE.get_or_init(|| ProbeRegexes {
        max_total_tokens: regex::Regex::new(r"max_total_tokens=(\d+)").unwrap(),
    })
}

/// Dynamically discovered model capabilities from NIM
#[derive(Debug, Clone)]
pub struct ModelCapabilities {
    pub(crate) max_total_tokens: u32,
    pub(crate) probed_at: std::time::Instant,
}

/// Cache for model capabilities, populated by probing NIM
pub type ModelCache = Arc<AsyncRwLock<HashMap<String, ModelCapabilities>>>;

pub(crate) const DEFAULT_CONTEXT_LIMIT: u32 = 131_072;
pub(crate) const CACHE_TTL_SECS: u64 = 3600; // re-probe every hour

/// Probe NIM to discover a model's max_total_tokens.
/// Technique: send max_tokens=999999 → NIM returns error revealing real limit.
pub(crate) async fn probe_model_limit(
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

    let mut req_builder = client
        .post(&url)
        .header("Content-Type", "application/json")
        .json(&probe_body)
        .timeout(std::time::Duration::from_secs(15));

    if let Some(key) = api_key {
        req_builder = req_builder.header("Authorization", format!("Bearer {}", key));
    }

    let resp = req_builder.send().await.ok()?;
    let body = resp.text().await.ok()?;

    // Parse "max_total_tokens=262144" from NIM error message
    let caps = probe_regexes().max_total_tokens.captures(&body)?;
    let limit: u32 = caps.get(1)?.as_str().parse().ok()?;

    tracing::info!("🔍 Probed model '{}': max_total_tokens = {}", model, limit);
    Some(limit)
}

/// Get context limit for a model, probing NIM if not cached.
pub async fn get_context_limit(
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
    let upstream = config
        .upstreams
        .get(upstream_name)
        .or_else(|| config.upstreams.get("default"));

    let (base_url, api_key) = match upstream {
        Some(u) => (u.base_url.clone(), u.api_key.as_deref()),
        None => (config.base_url.clone(), config.api_key.as_deref()),
    };

    // 3. Probe NIM
    if let Some(limit) = probe_model_limit(client, &base_url, api_key, model).await {
        let mut cache_write = cache.write().await;
        cache_write.insert(
            model.to_string(),
            ModelCapabilities {
                max_total_tokens: limit,
                probed_at: std::time::Instant::now(),
            },
        );
        return limit;
    }

    tracing::warn!(
        "⚠️ Could not probe model '{}', using default {}",
        model,
        DEFAULT_CONTEXT_LIMIT
    );
    DEFAULT_CONTEXT_LIMIT
}
