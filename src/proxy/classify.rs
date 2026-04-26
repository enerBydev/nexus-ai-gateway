use std::sync::OnceLock;

use crate::proxy::error_types::UpstreamError;
use crate::proxy::rate_limit::{is_l2_rate_limit, log_l2_rate_limit, L2_MIN_BACKOFF_MS};

// ═══════════════════════════════════════════════════════════════════════════
// Phase 1.3: OnceLock regex caching (avoids 10-50μs recompile per call)
// ═══════════════════════════════════════════════════════════════════════════

pub(crate) struct ErrorRegexes {
    pub(crate) input_tokens: regex::Regex,
    pub(crate) context_length: regex::Regex,
}

pub(crate) fn error_regexes() -> &'static ErrorRegexes {
    static RE: OnceLock<ErrorRegexes> = OnceLock::new();
    RE.get_or_init(|| ErrorRegexes {
        input_tokens: regex::Regex::new(r"passed\s+(\d+)\s+input\s+tokens").unwrap(),
        context_length: regex::Regex::new(r"context\s+length\s+is\s+only\s+(\d+)").unwrap(),
    })
}

pub(crate) const MAX_RETRIES: u32 = 3; // Reduced from 4: less cascade amplification (1.58x→~1.3x)
#[allow(dead_code)]
pub(crate) const RETRY_BASE_MS: u64 = 1500; // Kept for reference, overridden per error class
pub(crate) const MIN_CLAMP_TOKENS: u32 = 4096;

/// Error classification: 3 layers (Structural → Content-Aware → Status-Based)
#[derive(Debug)]
pub(crate) enum ErrorClass {
    /// Transient server error — retry with exponential backoff + jitter
    Retryable { base_delay_ms: u64, max_retries: u32, reason: &'static str },
    /// Parameter error — auto-correct (reduce max_tokens) and retry
    Fixable { reason: &'static str },
    /// Fatal error — return immediately to CC with Anthropic-native error type
    Fatal { reason: &'static str },
}

/// Content patterns that indicate a fixable error (max_tokens/context overflow)
pub(crate) const FIXABLE_PATTERNS: &[&str] = &[
    "max_tokens",
    "context_length",
    "too large",
    "maximum context length",
    "context window",
    "token limit",
    "exceeds the model",
    "input tokens",
    "reduce the length",
];

/// Content patterns that indicate a transient/retryable condition
pub(crate) const RETRYABLE_PATTERNS: &[&str] = &[
    "temporarily unavailable",
    "service unavailable",
    "model is loading",
    "try again",
    "capacity",
    "overloaded",
    "timeout",
    "connection reset",
    "econnreset",
    "upstream connect error",
    "degraded function",
    "cannot be invoked",
];

/// Content patterns that indicate a fatal/permanent error
pub(crate) const FATAL_PATTERNS: &[&str] = &[
    "invalid api key",
    "unauthorized",
    "forbidden",
    "not found",
    "invalid model",
    "does not exist",
    "deprecated",
    "billing",
    "quota exceeded",
];

/// v10.2: Extract safe max_tokens from NIM input_tokens overflow error.
///
/// NIM error format: "You passed {input} input tokens and requested {output} output tokens.
/// However, the model's context length is only {limit} tokens"
/// Returns: context_limit - real_input - safety_margin (256 tokens)
pub(crate) fn extract_safe_max_tokens_from_error(message: &str) -> Option<u32> {
    let real_input: u32 =
        error_regexes().input_tokens.captures(message)?.get(1)?.as_str().parse().ok()?;
    let context_limit: u32 =
        error_regexes().context_length.captures(message)?.get(1)?.as_str().parse().ok()?;

    // Safety margin: 256 tokens to guarantee fit across tokenizer differences
    let safe_max =
        context_limit.saturating_sub(real_input).saturating_sub(256).max(MIN_CLAMP_TOKENS);

    tracing::info!(
        "📐 Extracted from NIM error: input={}, limit={}, safe_max={}",
        real_input,
        context_limit,
        safe_max
    );
    Some(safe_max)
}

/// 3-layer error classification.
///
/// Layer 0: Structural — uses NIM's typed error fields (error.type, error.param)
/// Layer 1: Content-Aware — pattern matching on error message body
/// Layer 2: Status-Based — HTTP status code fallback
pub(crate) fn classify_error(upstream: &UpstreamError) -> ErrorClass {
    let lower = upstream.message.to_lowercase();

    // Check for L2 rate limit first (more specific)
    if is_l2_rate_limit(upstream) {
        log_l2_rate_limit("<model>", upstream);
        tracing::warn!(
            "⚠️ L2 rate limit detected: {} - using extended backoff",
            upstream.message.chars().take(100).collect::<String>()
        );
        return ErrorClass::Retryable {
            base_delay_ms: L2_MIN_BACKOFF_MS,
            max_retries: 3,
            reason: "L2 rate limit — provider concurrency cap",
        };
    }

    // ╔══════════════════════════════════════════════════════════╗
    // ║ LAYER 0: Structural — NIM typed error fields ║
    // ╚══════════════════════════════════════════════════════════╝
    if let Some(ref etype) = upstream.error_type {
        if etype.as_str() == "BadRequestError" && upstream.param.as_deref() == Some("input_tokens")
        {
            // v10.2: Try to auto-fix by reducing max_tokens to fit real input
            // Only fatal if we can't extract the real counts from the error
            if let Some(_safe_max) = extract_safe_max_tokens_from_error(&upstream.message) {
                return ErrorClass::Fixable {
                    reason: "input_tokens overflow — auto-clamping max_tokens to fit (L0)",
                };
            }
            return ErrorClass::Fatal {
                reason: "input_tokens overflow — context full, needs /compact (L0)",
            };
        }
    }

    // ╔══════════════════════════════════════════════════════════╗
    // ║ LAYER 1: Content-Aware Classification ║
    // ╚══════════════════════════════════════════════════════════╝
    for pattern in FATAL_PATTERNS {
        if lower.contains(pattern) {
            return ErrorClass::Fatal { reason: "fatal pattern in error body (L1)" };
        }
    }
    for pattern in FIXABLE_PATTERNS {
        if lower.contains(pattern) {
            return ErrorClass::Fixable {
                reason: "fixable pattern — token/context overflow (L1)"
            };
        }
    }
    for pattern in RETRYABLE_PATTERNS {
        if lower.contains(pattern) {
            return ErrorClass::Retryable {
                base_delay_ms: 2000,
                max_retries: 3,
                reason: "retryable pattern in error body (L1)",
            };
        }
    }

    // ╔══════════════════════════════════════════════════════════╗
    // ║ LAYER 2: Status-Based Classification ║
    // ╚══════════════════════════════════════════════════════════╝
    match upstream.status {
        429 => ErrorClass::Retryable {
            base_delay_ms: 10_000, // 10s — matches NIM recovery time
            max_retries: 3,
            reason: "429 rate limit — NIM concurrency cap (L2)",
        },
        500 => ErrorClass::Retryable {
            base_delay_ms: 3000,
            max_retries: 3,
            reason: "500 internal server error (L2)",
        },
        502 => ErrorClass::Retryable {
            base_delay_ms: 3000,
            max_retries: 3,
            reason: "502 bad gateway (L2)",
        },
        503 => ErrorClass::Retryable {
            base_delay_ms: 5000,
            max_retries: 4,
            reason: "503 service unavailable — model loading (L2)",
        },
        504 => ErrorClass::Retryable {
            base_delay_ms: 4000,
            max_retries: 3,
            reason: "504 gateway timeout (L2)",
        },
        529 => ErrorClass::Retryable {
            base_delay_ms: 5000,
            max_retries: 3,
            reason: "529 overloaded (L2)",
        },
        400 => ErrorClass::Fatal { reason: "400 bad request — no fixable pattern (L2)" },
        401 => ErrorClass::Fatal { reason: "401 unauthorized (L2)" },
        402 => ErrorClass::Fatal { reason: "402 billing error (L2)" },
        403 => ErrorClass::Fatal { reason: "403 forbidden (L2)" },
        404 => ErrorClass::Fatal { reason: "404 not found (L2)" },
        405 => ErrorClass::Fatal { reason: "405 method not allowed (L2)" },
        413 => ErrorClass::Fixable { reason: "413 payload too large (L2)" },
        422 => ErrorClass::Fatal { reason: "422 unprocessable entity (L2)" },
        // v0.11.0 (HI-04): HTTP 408 is a timeout — should be retried, not fatal
        408 => ErrorClass::Retryable {
            base_delay_ms: 5000,
            max_retries: 3,
            reason: "408 request timeout (L2)",
        },
        406..=407 | 409..=412 | 414..=421 | 423..=499 => {
            ErrorClass::Fatal { reason: "unknown 4xx client error (L2)" }
        }
        501 | 505..=528 | 530..=599 => ErrorClass::Retryable {
            base_delay_ms: 2000,
            max_retries: 2,
            reason: "unknown 5xx server error (L2)",
        },
        _ => ErrorClass::Fatal { reason: "unexpected status code (L2)" },
    }
}

/// Calculate delay with exponential backoff + jitter (avoids thundering herd)
/// Jitter range: ±25% of base, capped at 30s
pub(crate) fn delay_with_jitter(base_ms: u64, attempt: u32) -> u64 {
    use rand::Rng;
    let exponential = base_ms * 2u64.pow(attempt.saturating_sub(1));
    let capped = exponential.min(30_000);
    let jitter_range = capped / 4;
    let jitter = rand::thread_rng().gen_range(0..=(jitter_range * 2));
    capped.saturating_sub(jitter_range) + jitter
}
