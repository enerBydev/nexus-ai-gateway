use crate::proxy::error_types::UpstreamError;

/// L2 rate limit patterns from various providers
pub(crate) const L2_RATE_LIMIT_PATTERNS: &[&str] = &[
    "NIM concurrency cap (L2)",
    "concurrency limit exceeded",
    // REMOVED: "rate limit exceeded" - too broad, captures non-L2 rate limits
    "too many concurrent requests",
    "concurrent request limit",   // Alternative pattern
    "simultaneous request limit", // Alternative pattern
];

/// Check if error indicates L2 rate limit (provider-side concurrency)
pub(crate) fn is_l2_rate_limit(error: &UpstreamError) -> bool {
    L2_RATE_LIMIT_PATTERNS.iter().any(|pattern| {
        error
            .message
            .to_lowercase()
            .contains(&pattern.to_lowercase())
    })
}

/// L2 rate limit backoff configuration
/// Tracking: Future integration for advanced L2 backoff calculation (PHASE 3.5)
#[allow(dead_code)]
pub(crate) const L2_BACKOFF_MULTIPLIER: f64 = 2.0;
pub(crate) const L2_MIN_BACKOFF_MS: u64 = 2000;
/// Tracking: Future integration for advanced L2 backoff calculation (PHASE 3.5)
#[allow(dead_code)]
pub(crate) const L2_MAX_BACKOFF_MS: u64 = 30000;
/// Tracking: Future integration for advanced L2 backoff calculation (PHASE 3.5)
#[allow(dead_code)]
pub(crate) const L2_JITTER_PERCENT: f64 = 0.25;

/// Log L2 rate limit with actionable information
pub(crate) fn log_l2_rate_limit(model: &str, error: &UpstreamError) {
    tracing::warn!(
        target: "nexus::rate_limit",
        "🚫 L2_RATE_LIMIT model={} message=\"{}\"",
        model,
        error.message.chars().take(200).collect::<String>()
    );
    tracing::info!(
        target: "nexus::metrics",
        "METRIC rate_limit_l2_total{{model=\"{}\"}} 1",
        model
    );
}
