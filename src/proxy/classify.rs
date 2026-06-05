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

pub(crate) const MAX_RETRIES: u32 = 3; // Reduced from 4: less cascade amplification (1.58x->~1.3x)
#[allow(dead_code)]
pub(crate) const RETRY_BASE_MS: u64 = 1500; // Kept for reference, overridden per error class
pub(crate) const MIN_CLAMP_TOKENS: u32 = 4096;

/// Error classification: 3 layers (Structural -> Status-Guard -> Content-Aware -> Status-Based)
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
    // Issue #63/#88: Tool schema format mismatch (C8)
    "missing field `input_schema`",
    "missing field `parameters`",
    "failed to deserialize",
    // Issue #74/#88: Rate limit wrapped in error (C5)
    "rate_limit",
    "too many requests",
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
/// NOTE: These are ONLY applied when status code is in the 4xx range (non-retryable).
/// For 429/5xx, the status code is authoritative and L1 FATAL cannot override it.
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
        "[CALIB] Extracted from NIM error: input={}, limit={}, safe_max={}",
        real_input,
        context_limit,
        safe_max
    );
    Some(safe_max)
}

// ╔══════════════════════════════════════════════════════════════════════════╗
// ║ Issue #34: Redesigned error classification with status-code guards     ║
// ║                                                                        ║
// ║ NEW ORDER: L0 (Structural) -> StatusGuard (429/5xx) -> L1 (Content) -> L2║
// ║                                                                        ║
// ║ KEY INVARIANT: L1 FATAL_PATTERNS can NEVER override a retryable        ║
// ║ HTTP status code (429, 500-504, 529). The status code is the           ║
// ║ authoritative signal from the upstream server.                         ║
// ╚══════════════════════════════════════════════════════════════════════════╝

/// 3-layer error classification with status-code guards.
///
/// Layer 0: Structural — uses NIM's typed error fields (error.type, error.param)
/// Status Guard: For retryable status codes (429/5xx), bypass L1 FATAL_PATTERNS
/// Layer 1: Content-Aware — pattern matching on error message body
/// Layer 2: Status-Based — HTTP status code fallback
pub(crate) fn classify_error(upstream: &UpstreamError) -> ErrorClass {
    let lower = upstream.message.to_lowercase();
    let status = upstream.status;

    // ╔══════════════════════════════════════════════════════════╗
    // ║ LAYER 0: Structural — NIM typed error fields            ║
    // ║ Most precise: uses error.type + error.param             ║
    // ╚══════════════════════════════════════════════════════════╝
    if let Some(ref etype) = upstream.error_type {
        // Phase 2 (M8): Accept multi-provider error_type variants
        let is_structural =
            matches!(etype.as_str(), "BadRequestError" | "invalid_request_error" | "request_error");
        if is_structural && upstream.param.as_deref() == Some("input_tokens") {
            // v10.2: Try to auto-fix by reducing max_tokens to fit real input
            // Only fatal if we can't extract the real counts from the error
            if extract_safe_max_tokens_from_error(&upstream.message).is_some() {
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
    // ║ STATUS GUARD: Retryable status codes (429, 5xx)         ║
    // ║                                                         ║
    // ║ INVARIANT: L1 FATAL_PATTERNS cannot reach here.         ║
    // ║ The HTTP status code is the authoritative signal.       ║
    // ║ L1 can only REFINE within the Retryable class           ║
    // ║ (e.g., Retryable->Fixable), never escalate to Fatal.    ║
    // ║                                                         ║
    // ║ Fixes: M1, M2, M3, M4, M5, M6, M7                     ║
    // ╚══════════════════════════════════════════════════════════╝
    let is_retryable_status = matches!(status, 429 | 500..=504 | 529);

    if is_retryable_status {
        // L2 rate limit check (concurrency-specific patterns)
        if is_l2_rate_limit(upstream) {
            log_l2_rate_limit("<model>", upstream);
            tracing::warn!(
                "[WARN] L2 rate limit detected: {} - using extended backoff",
                upstream.message.chars().take(100).collect::<String>()
            );
            return ErrorClass::Retryable {
                base_delay_ms: L2_MIN_BACKOFF_MS,
                max_retries: 3,
                reason: "L2 rate limit — provider concurrency cap",
            };
        }

        // Within retryable status, L1 FIXABLE patterns CAN refine
        // (e.g., a 502 wrapping a token overflow should be Fixable)
        for pattern in FIXABLE_PATTERNS {
            if lower.contains(pattern) {
                return ErrorClass::Fixable {
                    reason: "fixable pattern in retryable status (L1+guard)",
                };
            }
        }

        // No L1 refinement — fall through to pure status classification
        return classify_by_status(status, &upstream.message);
    }

    // ╔══════════════════════════════════════════════════════════╗
    // ║ NON-RETRYABLE STATUS (4xx mostly)                       ║
    // ║ L1 content patterns apply fully here.                   ║
    // ║ FATAL_PATTERNS are safe to apply on 4xx.                ║
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
    // ║ LAYER 2: Status-Based Classification (fallback)         ║
    // ╚══════════════════════════════════════════════════════════╝
    classify_by_status(status, &upstream.message)
}

/// Pure status-code classification — extracted for reuse from both
/// the status guard path and the L2 fallback path.
fn classify_by_status(status: u16, error_message: &str) -> ErrorClass {
    match status {
        // Issue #34 Q1: Reduced from 3 to 1 retry for 429.
        // CC has its own 2 (SDK) + 10 (app) retries.
        // 1 proxy retry gives one chance to auto-heal without amplification.
        429 => ErrorClass::Retryable {
            base_delay_ms: 10_000, // 10s — matches NIM recovery time
            max_retries: 1,
            reason: "429 rate limit (L2)",
        },
        500 => ErrorClass::Retryable {
            base_delay_ms: 3000,
            max_retries: 3,
            reason: "500 internal server error (L2)",
        },
        502 => {
            // FIX C5 (Issue #74): NIM can wrap 429 in 502 — extract rate limit from body
            let lower_msg = error_message.to_lowercase();
            if lower_msg.contains("429")
                || lower_msg.contains("rate_limit")
                || lower_msg.contains("too many requests")
            {
                tracing::warn!("429 wrapped in 502 detected -- reclassifying as rate limit");
                ErrorClass::Retryable { base_delay_ms: 10000, max_retries: 1, reason: "429-in-502" }
            } else {
                ErrorClass::Retryable {
                    base_delay_ms: 3000,
                    max_retries: 3,
                    reason: "502 bad gateway (L2)",
                }
            }
        }
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
            max_retries: 4,
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
    use rand::RngExt;
    let exponential = base_ms * 2u64.pow(attempt.saturating_sub(1));
    let capped = exponential.min(30_000);
    let jitter_range = capped / 4;
    let jitter = rand::rng().random_range(0..=(jitter_range * 2));
    capped.saturating_sub(jitter_range) + jitter
}

#[cfg(test)]
#[path = "classify_test.rs"]
mod classify_test;
