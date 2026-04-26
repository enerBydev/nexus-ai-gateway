use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// Tracks overflow patterns per model to detect infinite loops.
/// 3+ consecutive overflows at the same token level (within 5%) -> force ContextOverflow.
pub(crate) struct OverflowLoopTracker;

#[derive(Debug, Clone)]
struct OverflowTracker {
    last_input_tokens: u32,
    consecutive_count: u32,
    last_timestamp: Instant,
}

static OVERFLOW_LOOP_TRACKER: OnceLock<Mutex<HashMap<String, OverflowTracker>>> = OnceLock::new();

/// Threshold: number of consecutive same-level overflows to trigger loop detection.
const LOOP_THRESHOLD: u32 = 3;

/// Variance: 5% tolerance for "same level" comparison.
const SAME_LEVEL_VARIANCE: f64 = 0.05;

fn get_trackers() -> &'static Mutex<HashMap<String, OverflowTracker>> {
    OVERFLOW_LOOP_TRACKER.get_or_init(|| Mutex::new(HashMap::new()))
}

impl OverflowLoopTracker {
    /// Check if this overflow is part of a loop.
    /// Returns true if ContextOverflow should be forced (3+ consecutive same-level overflows).
    pub fn check_overflow_loop(model: &str, input_tokens: u32) -> bool {
        if input_tokens == 0 {
            return false;
        }

        let mut trackers = get_trackers().lock().unwrap_or_else(|e| {
            tracing::error!("OverflowLoopTracker mutex poisoned: {}", e);
            e.into_inner()
        });

        let key = model.to_string();
        let mut triggered = false;

        trackers
            .entry(key)
            .and_modify(|tracker| {
                // Existing entry — compare against last recorded token level
                let within_variance = if tracker.last_input_tokens > 0 {
                    let diff = (input_tokens as i64 - tracker.last_input_tokens as i64)
                        .unsigned_abs() as f64;
                    let ratio = diff / tracker.last_input_tokens as f64;
                    ratio <= SAME_LEVEL_VARIANCE
                } else {
                    false
                };

                if within_variance {
                    tracker.last_input_tokens = input_tokens;
                    tracker.consecutive_count += 1;
                    tracker.last_timestamp = Instant::now();
                    if tracker.consecutive_count >= LOOP_THRESHOLD {
                        tracing::warn!(
                            "Overflow loop detected: {} consecutive overflows at ~{}K tokens for {}",
                            tracker.consecutive_count,
                            input_tokens / 1000,
                            model
                        );
                        // Reset after triggering to avoid repeated firing
                        tracker.consecutive_count = 0;
                        triggered = true;
                    }
                } else {
                    // Token level changed significantly — reset counter
                    tracker.last_input_tokens = input_tokens;
                    tracker.consecutive_count = 1;
                    tracker.last_timestamp = Instant::now();
                }
            })
            .or_insert(OverflowTracker {
                last_input_tokens: input_tokens,
                consecutive_count: 1,
                last_timestamp: Instant::now(),
            });

        triggered
    }

    /// Reset tracker for a model (call after successful non-overflow request).
    pub fn reset_tracker(model: &str) {
        let mut trackers = get_trackers().lock().unwrap_or_else(|e| {
            tracing::error!("OverflowLoopTracker mutex poisoned on reset: {}", e);
            e.into_inner()
        });
        if trackers.remove(model).is_some() {
            tracing::debug!("Overflow tracker reset for model {}", model);
        }
    }

    /// Reset all trackers (test utility only).
    #[cfg(test)]
    fn reset_all() {
        let mut trackers = get_trackers().lock().unwrap_or_else(|e| {
            tracing::error!("OverflowLoopTracker mutex poisoned on reset_all: {}", e);
            e.into_inner()
        });
        trackers.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_overflow_loop_detection() {
        OverflowLoopTracker::reset_all();
        let model = "test-model-loop";

        // First two overflows at same level should not trigger
        assert!(!OverflowLoopTracker::check_overflow_loop(model, 139_000));
        assert!(!OverflowLoopTracker::check_overflow_loop(model, 139_200));
        // Third overflow at same level should trigger
        assert!(OverflowLoopTracker::check_overflow_loop(model, 139_100));

        OverflowLoopTracker::reset_all();
    }

    #[test]
    fn test_different_models_tracked_separately() {
        OverflowLoopTracker::reset_all();
        let model_a = "test-model-sep-a";
        let model_b = "test-model-sep-b";

        // Accumulate overflows on model A only
        assert!(!OverflowLoopTracker::check_overflow_loop(model_a, 100_000));
        assert!(!OverflowLoopTracker::check_overflow_loop(model_a, 100_500));

        // Model B should start fresh — not triggered
        assert!(!OverflowLoopTracker::check_overflow_loop(model_b, 100_000));
        // Model A should trigger on third
        assert!(OverflowLoopTracker::check_overflow_loop(model_a, 100_200));

        OverflowLoopTracker::reset_all();
    }

    #[test]
    fn test_token_level_change_resets() {
        OverflowLoopTracker::reset_all();
        let model = "test-model-tok";

        // Two overflows at ~100K
        assert!(!OverflowLoopTracker::check_overflow_loop(model, 100_000));
        assert!(!OverflowLoopTracker::check_overflow_loop(model, 100_200));

        // Token level changes significantly (from 100K to 50K) — resets counter to 1
        assert!(!OverflowLoopTracker::check_overflow_loop(model, 50_000));
        // Second at new level
        assert!(!OverflowLoopTracker::check_overflow_loop(model, 50_100));
        // Third at new level should trigger
        assert!(OverflowLoopTracker::check_overflow_loop(model, 50_200));

        OverflowLoopTracker::reset_all();
    }

    #[test]
    fn test_reset_tracker() {
        OverflowLoopTracker::reset_all();
        let model = "test-model-manual";

        // Two overflows
        assert!(!OverflowLoopTracker::check_overflow_loop(model, 80_000));
        assert!(!OverflowLoopTracker::check_overflow_loop(model, 80_200));

        // Manual reset
        OverflowLoopTracker::reset_tracker(model);

        // Should start fresh — need 3 again
        assert!(!OverflowLoopTracker::check_overflow_loop(model, 80_000));
        assert!(!OverflowLoopTracker::check_overflow_loop(model, 80_100));
        assert!(OverflowLoopTracker::check_overflow_loop(model, 80_200));

        OverflowLoopTracker::reset_all();
    }

    #[test]
    fn test_within_5_percent_variance() {
        OverflowLoopTracker::reset_all();
        let model = "test-model-var";

        // 139000 and 139500 differ by ~0.36% — well within 5%
        assert!(!OverflowLoopTracker::check_overflow_loop(model, 139_000));
        assert!(!OverflowLoopTracker::check_overflow_loop(model, 139_500));

        // 139000 + 5% = 145950, so 145000 (~4.3%) is still within range
        assert!(OverflowLoopTracker::check_overflow_loop(model, 145_000));

        OverflowLoopTracker::reset_all();
    }

    #[test]
    fn test_zero_input_tokens_no_panic() {
        OverflowLoopTracker::reset_all();

        // Zero input tokens should not trigger and not panic
        assert!(!OverflowLoopTracker::check_overflow_loop("test-model-zero", 0));

        OverflowLoopTracker::reset_all();
    }

    #[test]
    fn test_triggers_then_resets_cleanly() {
        OverflowLoopTracker::reset_all();
        let model = "test-model-trig";

        // Trigger the loop detection
        assert!(!OverflowLoopTracker::check_overflow_loop(model, 200_000));
        assert!(!OverflowLoopTracker::check_overflow_loop(model, 200_100));
        assert!(OverflowLoopTracker::check_overflow_loop(model, 200_200));

        // After triggering (auto-reset), should need 3 again
        assert!(!OverflowLoopTracker::check_overflow_loop(model, 200_000));
        assert!(!OverflowLoopTracker::check_overflow_loop(model, 200_050));
        assert!(OverflowLoopTracker::check_overflow_loop(model, 200_100));

        OverflowLoopTracker::reset_all();
    }
}
