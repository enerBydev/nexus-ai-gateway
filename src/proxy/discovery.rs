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

/// Fallback context window used when runtime probing of a model fails.
///
/// Configurable via `PROBE_FALLBACK_CONTEXT_LIMIT` (default 200_000). The default
/// MUST be >= CC's context window (`CC_CONTEXT_WINDOW`, default 200_000) so that
/// `scale_token_usage` takes Branch 2 (report REAL tokens) instead of inflating
/// them. The previous fixed default of 131_072 was SMALLER than CC's effective
/// window (~180_000), so a probe failure forced a ~1.37x token inflation
/// (180_000 / 131_072) that made CC's context appear to fill within seconds and
/// fired a premature synthetic "context window full" error. Modern NIM models
/// (qwen, glm, deepseek, kimi) are >= 256K, so assuming 128K on a probe failure
/// was both wrong and harmful. Pin genuinely smaller models via
/// `MODEL_LIMIT_OVERRIDES` instead of lowering this fallback.
pub(crate) fn default_context_limit() -> u32 {
    const FALLBACK: u32 = 200_000;
    match std::env::var("PROBE_FALLBACK_CONTEXT_LIMIT") {
        // Unset is the normal case — silently use the default.
        Err(_) => FALLBACK,
        Ok(raw) => match raw.parse::<u32>() {
            Ok(v) if v > 0 => v,
            // Present but invalid (non-numeric or <= 0): warn so a misconfigured env
            // var is debuggable instead of silently masked, then use the default.
            _ => {
                tracing::warn!(
                    "[WARN] Invalid PROBE_FALLBACK_CONTEXT_LIMIT='{}' (expected positive integer) — using default {}",
                    raw,
                    FALLBACK
                );
                FALLBACK
            }
        },
    }
}

// FASE 3.6: Environment-variable configurable probe settings
fn cache_ttl_secs() -> u64 {
    std::env::var("PROBE_CACHE_TTL_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(3600)
}

fn probe_timeout_secs() -> u64 {
    std::env::var("PROBE_TIMEOUT_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(15)
}

fn probing_disabled() -> bool {
    std::env::var("DISABLE_PROBING")
        .map(|v| v == "1" || v.to_lowercase() == "true")
        .unwrap_or(false)
}

/// A context limit below this is almost certainly a parse error (Issue #106 §8): e.g. a
/// comma thousands-separator in `…:1,048,576` splits on `,` into `…:1` -> limit 1. Reject
/// such values rather than silently capping a model at a useless context.
const MIN_PLAUSIBLE_LIMIT: u32 = 1024;

/// Pure parser for `MODEL_LIMIT_OVERRIDES` (`model-id1:limit1,model-id2:limit2`). Returns
/// the override for `model_id`, ignoring implausibly small / non-numeric limits with a
/// warning so a malformed entry never silently wins.
fn parse_limit_override(overrides: &str, model_id: &str) -> Option<u32> {
    overrides.split(',').find_map(|entry| {
        let mut parts = entry.trim().split(':');
        let model = parts.next()?;
        if model != model_id {
            return None;
        }
        let raw = parts.next()?.trim();
        match raw.parse::<u32>() {
            Ok(limit) if limit >= MIN_PLAUSIBLE_LIMIT => Some(limit),
            Ok(limit) => {
                tracing::warn!(
                    "MODEL_LIMIT_OVERRIDES: ignoring implausibly small limit {} for '{}' (min {}). \
                     Check for comma thousands-separators — use 1048576, not 1,048,576.",
                    limit, model_id, MIN_PLAUSIBLE_LIMIT
                );
                None
            }
            Err(e) => {
                tracing::warn!(
                    "MODEL_LIMIT_OVERRIDES: invalid limit '{}' for '{}' ({}) — ignoring.",
                    raw,
                    model_id,
                    e
                );
                None
            }
        }
    })
}

/// Get model limit override from the `MODEL_LIMIT_OVERRIDES` env var.
fn get_model_limit_override(model_id: &str) -> Option<u32> {
    std::env::var("MODEL_LIMIT_OVERRIDES").ok().and_then(|o| parse_limit_override(&o, model_id))
}

/// Probe NIM to discover a model's max_total_tokens.
/// Technique: send max_tokens=999999 -> NIM returns error revealing real limit.
pub(crate) async fn probe_model_limit(
    client: &Client,
    base_url: &str,
    api_key: Option<&str>,
    model: &str,
) -> Option<u32> {
    // FASE 3.6: Check if probing is disabled
    if probing_disabled() {
        tracing::info!("[SCAN] Probing disabled via DISABLE_PROBING");
        return None;
    }

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
        .timeout(std::time::Duration::from_secs(probe_timeout_secs()));

    if let Some(key) = api_key {
        req_builder = req_builder.header("Authorization", format!("Bearer {}", key));
    }

    let resp = req_builder.send().await.ok()?;
    let body = resp.text().await.ok()?;

    // Parse "max_total_tokens=262144" from NIM error message
    let caps = probe_regexes().max_total_tokens.captures(&body)?;
    let limit: u32 = caps.get(1)?.as_str().parse().ok()?;

    tracing::info!("[SCAN] Probed model '{}': max_total_tokens = {}", model, limit);
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
    // FASE 3.6: Check MODEL_LIMIT_OVERRIDES first
    if let Some(override_limit) = get_model_limit_override(model) {
        tracing::info!("[SCAN] Model limit override for {}: {}", model, override_limit);
        return override_limit;
    }

    // 1. Check cache
    {
        let cache_read = cache.read().await;
        if let Some(caps) = cache_read.get(model) {
            if caps.probed_at.elapsed().as_secs() < cache_ttl_secs() {
                return caps.max_total_tokens;
            }
        }
    }

    // Issue #104: NVIDIA NIM silently clamps an oversized max_tokens (HTTP 200, no
    // `max_total_tokens=` error string), so the error-parsing probe never succeeds — it only
    // burns a round-trip + PROBE_TIMEOUT and re-warns on every request (there is no negative
    // cache). Skip it for NIM and use the fallback; pin real limits via MODEL_LIMIT_OVERRIDES.
    // Non-NIM upstreams still probe.
    if config.get_upstream_type(upstream_name) == crate::config::UpstreamType::NIM {
        let fallback = default_context_limit();
        tracing::debug!(
            "[SCAN] NIM probe skipped for '{}' (silent clamp, Issue #104) — using fallback {}. Pin via MODEL_LIMIT_OVERRIDES.",
            model,
            fallback
        );
        return fallback;
    }

    // 2. Get upstream base URL (without /v1/chat/completions)
    let upstream = config.upstreams.get(upstream_name).or_else(|| config.upstreams.get("default"));

    let (base_url, api_key) = match upstream {
        Some(u) => (u.base_url.clone(), u.api_key.as_deref()),
        None => (config.base_url.clone(), config.api_key.as_deref()),
    };

    // 3. Probe NIM
    if let Some(limit) = probe_model_limit(client, &base_url, api_key, model).await {
        let mut cache_write = cache.write().await;
        cache_write.insert(
            model.to_string(),
            ModelCapabilities { max_total_tokens: limit, probed_at: std::time::Instant::now() },
        );
        return limit;
    }

    let fallback = default_context_limit();
    tracing::warn!(
        "[WARN] Could not probe model '{}', using fallback {} (tune via PROBE_FALLBACK_CONTEXT_LIMIT)",
        model,
        fallback
    );
    fallback
}

#[cfg(test)]
mod fallback_tests {
    use super::default_context_limit;

    /// Save/restore the single (uniquely-named) env var this module touches so the
    /// test is order-independent. Only this test reads/writes this key, so there is
    /// no cross-test race on the process-wide environment.
    fn with_env<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
        let key = "PROBE_FALLBACK_CONTEXT_LIMIT";
        let prev = std::env::var(key).ok();
        match value {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        let out = f();
        match prev {
            Some(p) => std::env::set_var(key, p),
            None => std::env::remove_var(key),
        }
        out
    }

    #[test]
    fn probe_fallback_default_and_overrides() {
        // Default 200_000 and >= CC's effective window (~180K) so a probe failure
        // never pushes scale_token_usage into its inflating Branch 1 — the root of
        // the "context fills in 2 seconds" bug. Modern NIM models are >= 256K, so
        // the old 128K default was both wrong and harmful.
        let d = with_env(None, default_context_limit);
        assert_eq!(d, 200_000, "default fallback should be 200_000");
        assert!(d >= 180_000, "fallback must be >= CC effective window (no inflation)");
        // Env override is honoured.
        assert_eq!(with_env(Some("262144"), default_context_limit), 262_144);
        // Invalid values fall back to the default.
        assert_eq!(with_env(Some("0"), default_context_limit), 200_000);
        assert_eq!(with_env(Some("garbage"), default_context_limit), 200_000);
    }
}

#[cfg(test)]
mod limit_override_tests {
    use super::parse_limit_override;

    #[test]
    fn valid_override_and_model_selection() {
        let s = "z-ai/glm-5.1:202752,moonshotai/kimi-k2.6:262144";
        assert_eq!(parse_limit_override(s, "z-ai/glm-5.1"), Some(202752));
        assert_eq!(parse_limit_override(s, "moonshotai/kimi-k2.6"), Some(262144));
        assert_eq!(parse_limit_override(s, "other/model"), None);
    }

    #[test]
    fn rejects_comma_thousands_separator_artifact() {
        // Issue #106 §8: "deepseek-ai/deepseek-v4-pro:1,048,576" splits on ',' so the
        // matching entry becomes "…:1" (limit 1) — must be rejected, never silently cap
        // the model at 1 token. Surrounding good entries stay unaffected.
        let s =
            "z-ai/glm-5.1:202752,deepseek-ai/deepseek-v4-pro:1,048,576,moonshotai/kimi-k2.6:262144";
        assert_eq!(parse_limit_override(s, "deepseek-ai/deepseek-v4-pro"), None);
        assert_eq!(parse_limit_override(s, "z-ai/glm-5.1"), Some(202752));
        assert_eq!(parse_limit_override(s, "moonshotai/kimi-k2.6"), Some(262144));
    }

    #[test]
    fn rejects_zero_and_nonnumeric() {
        assert_eq!(parse_limit_override("m:0", "m"), None);
        assert_eq!(parse_limit_override("m:abc", "m"), None);
        assert_eq!(parse_limit_override("m:1023", "m"), None, "below MIN_PLAUSIBLE_LIMIT");
        assert_eq!(parse_limit_override("m:1024", "m"), Some(1024), "exactly MIN is allowed");
    }
}
