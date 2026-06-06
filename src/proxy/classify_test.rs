// ╔══════════════════════════════════════════════════════════════════════════╗
// ║ Issue #34: Comprehensive unit tests for classify_error()               ║
// ║ Covers all 10 misclassification scenarios (M1-M10) plus edge cases     ║
// ╚══════════════════════════════════════════════════════════════════════════╝

use super::*;
use crate::proxy::error_types::UpstreamError;

fn make_error(status: u16, msg: &str) -> UpstreamError {
    UpstreamError { status, message: msg.to_string(), error_type: None, param: None, code: None }
}

fn make_typed_error(status: u16, msg: &str, etype: &str, param: &str) -> UpstreamError {
    UpstreamError {
        status,
        message: msg.to_string(),
        error_type: Some(etype.to_string()),
        param: Some(param.to_string()),
        code: None,
    }
}

// ═══════════════════════════════════════════════════════════════
// M1-M7: Status Guard — CRITICAL + HIGH scenarios
// These verify that L1 FATAL_PATTERNS cannot override retryable
// status codes (429, 5xx).
// ═══════════════════════════════════════════════════════════════

#[test]
fn m1_429_quota_exceeded_is_retryable() {
    let err = make_error(429, "quota exceeded for this API key");
    let class = classify_error(&err);
    assert!(
        matches!(class, ErrorClass::Retryable { .. }),
        "M1: 429 + 'quota exceeded' must be Retryable, got {:?}",
        class
    );
}

#[test]
fn m2_429_capacity_is_retryable_with_proper_backoff() {
    let err = make_error(429, "Server at capacity, try again later");
    match classify_error(&err) {
        ErrorClass::Retryable { base_delay_ms, .. } => {
            assert!(
                base_delay_ms >= 10_000,
                "M2: 429+'capacity' backoff should be >= 10s, got {}ms",
                base_delay_ms
            );
        }
        other => panic!("M2: Expected Retryable, got {:?}", other),
    }
}

#[test]
fn m3_429_overloaded_is_retryable_with_proper_backoff() {
    let err = make_error(429, "Service overloaded, please wait");
    match classify_error(&err) {
        ErrorClass::Retryable { base_delay_ms, .. } => {
            assert!(
                base_delay_ms >= 10_000,
                "M3: 429+'overloaded' backoff should be >= 10s, got {}ms",
                base_delay_ms
            );
        }
        other => panic!("M3: Expected Retryable, got {:?}", other),
    }
}

#[test]
fn m4_503_overloaded_is_retryable_5s_4retries() {
    let err = make_error(503, "Service overloaded");
    match classify_error(&err) {
        ErrorClass::Retryable { base_delay_ms, max_retries, .. } => {
            assert!(base_delay_ms >= 5000, "M4: 503 backoff >= 5s");
            assert_eq!(max_retries, 4, "M4: 503 should have 4 retries");
        }
        other => panic!("M4: Expected Retryable, got {:?}", other),
    }
}

#[test]
fn m5_503_not_found_in_body_is_retryable_not_fatal() {
    let err = make_error(503, "service not found in registry");
    let class = classify_error(&err);
    assert!(
        matches!(class, ErrorClass::Retryable { .. }),
        "M5: 503 + 'not found' must be Retryable (status > body), got {:?}",
        class
    );
}

#[test]
fn m6_529_billing_in_body_is_retryable_not_fatal() {
    let err = make_error(529, "billing quota temporarily exceeded");
    let class = classify_error(&err);
    assert!(
        matches!(class, ErrorClass::Retryable { .. }),
        "M6: 529 + 'billing' must be Retryable (status > body), got {:?}",
        class
    );
}

#[test]
fn m7_429_unauthorized_in_body_is_retryable_not_fatal() {
    let err = make_error(429, "Unauthorized to access model while rate limited");
    let class = classify_error(&err);
    assert!(
        matches!(class, ErrorClass::Retryable { .. }),
        "M7: 429 + 'unauthorized' must be Retryable, got {:?}",
        class
    );
}

// ═══════════════════════════════════════════════════════════════
// M8: L0 multi-provider error_type matching
// ═══════════════════════════════════════════════════════════════

#[test]
fn m8_invalid_request_error_with_input_tokens_is_fixable() {
    let msg = "You passed 130000 input tokens and requested 64000 output tokens. However, the model's context length is only 131072 tokens";
    let err = make_typed_error(400, msg, "invalid_request_error", "input_tokens");
    let class = classify_error(&err);
    assert!(
        matches!(class, ErrorClass::Fixable { .. }),
        "M8: invalid_request_error + input_tokens must be Fixable(L0), got {:?}",
        class
    );
}

#[test]
fn m8_bad_request_error_with_input_tokens_is_fixable() {
    let msg = "You passed 130000 input tokens and requested 64000 output tokens. However, the model's context length is only 131072 tokens";
    let err = make_typed_error(400, msg, "BadRequestError", "input_tokens");
    let class = classify_error(&err);
    assert!(
        matches!(class, ErrorClass::Fixable { .. }),
        "M8: BadRequestError + input_tokens must be Fixable(L0), got {:?}",
        class
    );
}

#[test]
fn m8_request_error_with_input_tokens_is_fixable() {
    let msg = "You passed 130000 input tokens and requested 64000 output tokens. However, the model's context length is only 131072 tokens";
    let err = make_typed_error(400, msg, "request_error", "input_tokens");
    let class = classify_error(&err);
    assert!(
        matches!(class, ErrorClass::Fixable { .. }),
        "M8: request_error + input_tokens must be Fixable(L0), got {:?}",
        class
    );
}

// ═══════════════════════════════════════════════════════════════
// M10: L2 rate limit backoff alignment
// ═══════════════════════════════════════════════════════════════

#[test]
fn m10_l2_rate_limit_uses_aligned_backoff() {
    let err = make_error(429, "concurrency limit exceeded");
    match classify_error(&err) {
        ErrorClass::Retryable { base_delay_ms, .. } => {
            assert!(
                base_delay_ms >= 10_000,
                "M10: L2 rate limit backoff should be >= 10s, got {}ms",
                base_delay_ms
            );
        }
        other => panic!("M10: Expected Retryable, got {:?}", other),
    }
}

// ═══════════════════════════════════════════════════════════════
// Guard tests: L1 Fatal NEVER on 5xx/429
// Every FATAL_PATTERN tested against every retryable status
// ═══════════════════════════════════════════════════════════════

#[test]
fn guard_500_with_fatal_patterns_is_retryable() {
    for pattern in FATAL_PATTERNS {
        let err = make_error(500, pattern);
        let class = classify_error(&err);
        assert!(
            matches!(class, ErrorClass::Retryable { .. }),
            "Guard: 500 + '{}' must be Retryable, got {:?}",
            pattern,
            class
        );
    }
}

#[test]
fn guard_502_with_fatal_patterns_is_retryable() {
    for pattern in FATAL_PATTERNS {
        let err = make_error(502, pattern);
        let class = classify_error(&err);
        assert!(
            matches!(class, ErrorClass::Retryable { .. }),
            "Guard: 502 + '{}' must be Retryable, got {:?}",
            pattern,
            class
        );
    }
}

#[test]
fn guard_529_with_fatal_patterns_is_retryable() {
    for pattern in FATAL_PATTERNS {
        let err = make_error(529, pattern);
        let class = classify_error(&err);
        assert!(
            matches!(class, ErrorClass::Retryable { .. }),
            "Guard: 529 + '{}' must be Retryable, got {:?}",
            pattern,
            class
        );
    }
}

// ═══════════════════════════════════════════════════════════════
// L1 Normal: Fatal correct on 4xx
// ═══════════════════════════════════════════════════════════════

#[test]
fn l1_400_unauthorized_is_fatal() {
    let err = make_error(400, "unauthorized access denied");
    assert!(matches!(classify_error(&err), ErrorClass::Fatal { .. }));
}

#[test]
fn l1_403_quota_exceeded_is_fatal() {
    let err = make_error(403, "quota exceeded permanently");
    assert!(matches!(classify_error(&err), ErrorClass::Fatal { .. }));
}

#[test]
fn l1_402_billing_is_fatal() {
    let err = make_error(402, "billing account suspended");
    assert!(matches!(classify_error(&err), ErrorClass::Fatal { .. }));
}

// ═══════════════════════════════════════════════════════════════
// L2 Fallback: Pure status code classification
// ═══════════════════════════════════════════════════════════════

#[test]
fn l2_429_clean_is_retryable() {
    let err = make_error(429, "plain rate limit message");
    assert!(matches!(classify_error(&err), ErrorClass::Retryable { .. }));
}

#[test]
fn l2_503_clean_is_retryable_4_retries() {
    let err = make_error(503, "plain service unavailable");
    match classify_error(&err) {
        ErrorClass::Retryable { max_retries, .. } => {
            assert_eq!(max_retries, 4, "503 should have 4 retries");
        }
        other => panic!("Expected Retryable, got {:?}", other),
    }
}

#[test]
fn l2_529_clean_is_retryable_4_retries() {
    let err = make_error(529, "overloaded");
    match classify_error(&err) {
        ErrorClass::Retryable { max_retries, .. } => {
            assert_eq!(max_retries, 4, "529 should have 4 retries");
        }
        other => panic!("Expected Retryable, got {:?}", other),
    }
}

#[test]
fn l2_400_clean_is_fatal() {
    let err = make_error(400, "some unknown 400 error");
    assert!(matches!(classify_error(&err), ErrorClass::Fatal { .. }));
}

#[test]
fn l2_404_clean_is_fatal() {
    let err = make_error(404, "endpoint not available");
    assert!(matches!(classify_error(&err), ErrorClass::Fatal { .. }));
}

#[test]
fn l2_408_is_retryable() {
    let err = make_error(408, "request timed out");
    assert!(matches!(classify_error(&err), ErrorClass::Retryable { .. }));
}

// ═══════════════════════════════════════════════════════════════
// Edge Cases
// ═══════════════════════════════════════════════════════════════

#[test]
fn extract_safe_max_tokens_nim_format() {
    let msg = "You passed 130000 input tokens and requested 64000 output tokens. However, the model's context length is only 131072 tokens";
    let safe = extract_safe_max_tokens_from_error(msg);
    assert!(safe.is_some(), "Should extract safe max from NIM format");
    let val = safe.unwrap();
    // 131072 - 130000 - 256 = 816 -> clamped to MIN_CLAMP_TOKENS (4096)
    assert_eq!(val, MIN_CLAMP_TOKENS);
}

#[test]
fn extract_safe_max_tokens_sufficient_room() {
    let msg = "You passed 50000 input tokens and requested 64000 output tokens. However, the model's context length is only 131072 tokens";
    let safe = extract_safe_max_tokens_from_error(msg);
    assert!(safe.is_some());
    let val = safe.unwrap();
    // 131072 - 50000 - 256 = 80816
    assert_eq!(val, 80816);
}

#[test]
fn extract_safe_max_tokens_missing_fields_returns_none() {
    let msg = "Some generic error without token counts";
    assert!(extract_safe_max_tokens_from_error(msg).is_none());
}

#[test]
fn delay_with_jitter_first_attempt_near_base() {
    // First attempt (attempt=1) should produce delay near base_ms
    let delay = delay_with_jitter(10_000, 1);
    // With ±25% jitter: range is 7500..12500
    assert!(delay >= 7500 && delay <= 12500, "Delay {} out of expected range", delay);
}

#[test]
fn delay_with_jitter_caps_at_30s() {
    // High attempt should cap at 30s even with large base
    let delay = delay_with_jitter(10_000, 10);
    assert!(delay <= 37500, "Delay {} should be capped near 30s", delay);
}

#[test]
fn l2_429_reduced_retries() {
    // Issue #34 Q1: 429 should have max_retries=1
    let err = make_error(429, "rate limit hit");
    match classify_error(&err) {
        ErrorClass::Retryable { max_retries, .. } => {
            assert_eq!(max_retries, 1, "429 should have 1 retry (Q1)");
        }
        other => panic!("Expected Retryable, got {:?}", other),
    }
}

#[test]
fn fixable_within_retryable_status() {
    // A 502 wrapping a token overflow should be Fixable
    let err = make_error(502, "maximum context length exceeded");
    let class = classify_error(&err);
    assert!(
        matches!(class, ErrorClass::Fixable { .. }),
        "502 + fixable pattern should be Fixable, got {:?}",
        class
    );
}
