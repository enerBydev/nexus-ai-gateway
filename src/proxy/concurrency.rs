use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock as AsyncRwLock;
use tokio::sync::Semaphore;

use crate::circuit_breaker;
use crate::error::{ProxyError, ProxyResult};

/// Per-model concurrency slot (Issue #59): the semaphore gating in-flight requests,
/// plus an atomic counter of requests currently attempting to acquire a permit for this
/// model. Both share one map entry so a model's semaphore and its queue-depth counter
/// always share the same lifecycle (created together, looked up together).
#[derive(Clone)]
pub(crate) struct ModelSlot {
    semaphore: Arc<Semaphore>,
    queue_depth: Arc<AtomicUsize>,
}

/// Shared collection of per-model semaphores (+ queue depth, Issue #59).
pub type ModelSemaphores = Arc<AsyncRwLock<HashMap<String, ModelSlot>>>;

/// Result of successfully acquiring a per-model concurrency permit (Issue #59). Carries
/// capacity/backpressure info so callers can attach `x-ratelimit-*` response headers —
/// including on SUCCESSFUL responses, so Claude Code can self-throttle before it ever
/// hits the limit.
pub(crate) struct AcquireResult {
    pub(crate) permit: tokio::sync::OwnedSemaphorePermit,
    /// Semaphore permits free for this model immediately after acquiring ours.
    pub(crate) available_permits: usize,
    /// Requests currently waiting for a permit for this model (excludes this one — it
    /// just got its permit and is no longer "queued").
    pub(crate) queue_depth: usize,
}

/// Floor, ceiling, and per-waiter increment for the `retry-after` suggested on a 503
/// Overloaded response (Issue #59) — scales with backlog instead of a flat 5s.
const RETRY_AFTER_FLOOR_SECS: u64 = 5;
const RETRY_AFTER_CEILING_SECS: u64 = 60;
const RETRY_AFTER_PER_WAITER_SECS: u64 = 2;

/// Suggest a `retry-after` value (seconds) proportional to how backed up a model's
/// queue is, instead of a flat 5s regardless of load (Issue #59).
fn suggested_retry_after_secs(queue_depth: usize) -> u64 {
    (RETRY_AFTER_FLOOR_SECS + queue_depth as u64 * RETRY_AFTER_PER_WAITER_SECS)
        .min(RETRY_AFTER_CEILING_SECS)
}

/// Insert the Issue #59 backpressure headers into a response: `x-ratelimit-remaining`
/// (free permits for this model) and `x-ratelimit-queue-depth` (current waiters).
/// Attached to ALL responses for a model — including successful ones — so a client can
/// see how close a model is to its concurrency limit and self-throttle before hitting it.
pub(crate) fn insert_ratelimit_headers(
    headers: &mut axum::http::HeaderMap,
    available_permits: usize,
    queue_depth: usize,
) {
    if let Ok(v) = available_permits.to_string().parse() {
        headers.insert("x-ratelimit-remaining", v);
    }
    if let Ok(v) = queue_depth.to_string().parse() {
        headers.insert("x-ratelimit-queue-depth", v);
    }
}

/// Issue #73: Per-model circuit breaker registry. One `CircuitBreaker` per upstream model
/// (keyed by the upstream model id, not the Claude alias). A failure in model A no longer
/// trips the breaker for model B.
pub struct CircuitBreakerMap {
    enabled: bool,
    threshold: u32,
    recovery_timeout: Duration,
    breakers: AsyncRwLock<HashMap<String, Arc<circuit_breaker::CircuitBreaker>>>,
}

impl CircuitBreakerMap {
    pub fn new(enabled: bool, threshold: u32, recovery_secs: u64) -> Self {
        Self {
            enabled,
            threshold,
            recovery_timeout: Duration::from_secs(recovery_secs),
            breakers: AsyncRwLock::new(HashMap::new()),
        }
    }

    /// Maximum distinct models tracked. Beyond this, new models get the most recent
    /// breaker (prevents unbounded memory growth from unknown client model names).
    const MAX_MODELS: usize = 256;

    /// Get or create the circuit breaker for a specific upstream model. When the registry
    /// is disabled, returns a shared no-op breaker (no allocation per call).
    pub async fn get(&self, model: &str) -> Arc<circuit_breaker::CircuitBreaker> {
        if !self.enabled {
            static DISABLED: std::sync::OnceLock<Arc<circuit_breaker::CircuitBreaker>> =
                std::sync::OnceLock::new();
            return DISABLED
                .get_or_init(|| Arc::new(circuit_breaker::CircuitBreaker::disabled()))
                .clone();
        }

        // Fast path: read lock
        {
            let read = self.breakers.read().await;
            if let Some(cb) = read.get(model) {
                return cb.clone();
            }
        }

        // Slow path: write lock to create
        let mut write = self.breakers.write().await;

        // Guard: cap the map to prevent unbounded growth from unknown model names.
        // Once at capacity, return a shared default breaker for any new model.
        if write.len() >= Self::MAX_MODELS && !write.contains_key(model) {
            tracing::warn!(
                "[CB] Per-model breaker map at capacity ({}) — using shared breaker for '{}'",
                Self::MAX_MODELS,
                model
            );
            static OVERFLOW: std::sync::OnceLock<Arc<circuit_breaker::CircuitBreaker>> =
                std::sync::OnceLock::new();
            return OVERFLOW
                .get_or_init(|| Arc::new(circuit_breaker::CircuitBreaker::default()))
                .clone();
        }

        write
            .entry(model.to_string())
            .or_insert_with(|| {
                tracing::info!(
                    "[CB] Created per-model circuit breaker for '{}' (threshold={}, recovery={}s)",
                    model,
                    self.threshold,
                    self.recovery_timeout.as_secs()
                );
                Arc::new(circuit_breaker::CircuitBreaker::new(
                    self.threshold,
                    self.recovery_timeout,
                ))
            })
            .clone()
    }
}

pub type CircuitBreaker = Arc<CircuitBreakerMap>;

/// Acquire a concurrency permit for a specific NIM model.
///
/// Issue #59: also enforces a queue-depth limit. If more than `max_queue_depth` requests
/// are already attempting to acquire a permit for this model, this call is rejected
/// immediately (503) instead of waiting up to `permit_timeout`s behind a backlog that was
/// never going to drain in time — a thundering-herd guard.
pub(crate) async fn acquire_model_permit(
    semaphores: &ModelSemaphores,
    model: &str,
    max_concurrent: usize,
    permit_timeout: u64,
    max_queue_depth: usize,
) -> ProxyResult<AcquireResult> {
    let slot = {
        let read = semaphores.read().await;
        if let Some(s) = read.get(model) {
            s.clone()
        } else {
            drop(read);
            let mut write = semaphores.write().await;
            write
                .entry(model.to_string())
                .or_insert_with(|| {
                    tracing::info!(
                        "[GUARD] Created concurrency semaphore for '{}' ({} permits, max_queue_depth={})",
                        model,
                        max_concurrent,
                        max_queue_depth,
                    );
                    ModelSlot {
                        semaphore: Arc::new(Semaphore::new(max_concurrent)),
                        queue_depth: Arc::new(AtomicUsize::new(0)),
                    }
                })
                .clone()
        }
    };

    // Issue #59: count this request as a waiter BEFORE checking the limit, then reject
    // immediately if the queue is already saturated — no point waiting up to
    // `permit_timeout`s behind a backlog that was never going to clear in time.
    let waiters = slot.queue_depth.fetch_add(1, Ordering::Relaxed) + 1;
    if waiters > max_queue_depth {
        slot.queue_depth.fetch_sub(1, Ordering::Relaxed);
        tracing::warn!(
            "🚦 Queue depth exceeded for '{}': {} waiters > limit {} — rejecting immediately",
            model,
            waiters,
            max_queue_depth,
        );
        return Err(ProxyError::Overloaded(
            format!(
                "Model '{}' request queue is full ({} waiters, limit {}) — try again shortly",
                model, waiters, max_queue_depth,
            ),
            Some(suggested_retry_after_secs(waiters)),
        ));
    }

    let available = slot.semaphore.available_permits();
    if available == 0 {
        tracing::warn!(
            "⏳ Model '{}' at capacity (0/{} permits, {} waiting) — waiting up to {}s",
            model,
            max_concurrent,
            waiters,
            permit_timeout,
        );
    } else {
        tracing::debug!(
            "🎫 Acquiring permit for '{}' ({}/{} available, {} waiting)",
            model,
            available,
            max_concurrent,
            waiters,
        );
    }

    let acquired = tokio::time::timeout(
        std::time::Duration::from_secs(permit_timeout),
        slot.semaphore.clone().acquire_owned(),
    )
    .await;

    // No longer queued, regardless of outcome — decrement on every exit path.
    slot.queue_depth.fetch_sub(1, Ordering::Relaxed);

    match acquired {
        Ok(Ok(permit)) => {
            let available_permits = slot.semaphore.available_permits();
            let queue_depth = slot.queue_depth.load(Ordering::Relaxed);
            tracing::debug!(
                "🎫 Permit acquired for '{}' ({}/{} remaining, {} still waiting)",
                model,
                available_permits,
                max_concurrent,
                queue_depth,
            );
            Ok(AcquireResult { permit, available_permits, queue_depth })
        }
        Ok(Err(_)) => {
            tracing::error!("[GUARD] Semaphore CLOSED for '{}' — this is a bug", model);
            Err(ProxyError::Internal(format!("Semaphore closed for '{}'", model)))
        }
        Err(_) => {
            tracing::error!(
                "⏰ Permit TIMEOUT for '{}' (waited {}s, 0/{} available)",
                model,
                permit_timeout,
                max_concurrent,
            );
            Err(ProxyError::Overloaded(
                format!(
                    "Model '{}' concurrency limit reached ({} slots busy for {}s)",
                    model, max_concurrent, permit_timeout,
                ),
                Some(suggested_retry_after_secs(waiters)),
            ))
        }
    }
}

#[cfg(test)]
mod queue_depth_tests {
    use super::*;

    fn new_semaphores() -> ModelSemaphores {
        Arc::new(AsyncRwLock::new(HashMap::new()))
    }

    #[tokio::test]
    async fn test_acquire_result_contains_queue_info_when_uncontended() {
        let semaphores = new_semaphores();
        let result = acquire_model_permit(&semaphores, "model-a", 5, 5, 20).await.unwrap();
        // 5 permits total, we hold 1 -> 4 free.
        assert_eq!(result.available_permits, 4);
        // No one else was waiting.
        assert_eq!(result.queue_depth, 0);
    }

    #[tokio::test]
    async fn test_queue_depth_limit_rejects_immediately_when_saturated() {
        let semaphores = new_semaphores();
        // max_queue_depth=0: even the very first waiter is already over the limit, so
        // this must fail immediately rather than waiting on the (otherwise free) semaphore.
        let err = acquire_model_permit(&semaphores, "model-b", 5, 5, 0).await;
        match err {
            Err(ProxyError::Overloaded(msg, retry_after)) => {
                assert!(msg.contains("queue is full"), "unexpected message: {msg}");
                // This request counts itself as waiter #1 -> 5 (floor) + 1*2 = 7s.
                assert_eq!(retry_after, Some(7), "1 waiter -> floor(5) + 1*2 = 7s");
            }
            other => panic!("expected ProxyError::Overloaded, got {:?}", other.map(|_| ())),
        }
    }

    #[tokio::test]
    async fn test_third_request_rejected_while_second_is_queued() {
        let semaphores = new_semaphores();

        // Capacity 1: the first call takes the only permit and holds it.
        let first = acquire_model_permit(&semaphores, "model-c", 1, 5, 5).await.unwrap();
        assert_eq!(first.queue_depth, 0);

        // Second call: semaphore is exhausted, so it genuinely blocks. max_queue_depth=5
        // has room for it (waiters=1), so it must wait rather than reject.
        let sem2 = semaphores.clone();
        let mut waiter =
            tokio::spawn(async move { acquire_model_permit(&sem2, "model-c", 1, 5, 5).await });
        // Give the scheduler a chance to poll the spawned task at least once so it
        // registers as a waiter before we check the depth limit below.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished(), "second call should still be queued behind the permit");

        // Third call with max_queue_depth=1: the second call is already 1 waiter, so this
        // one would be waiter #2 — must be rejected immediately, without ever touching the
        // semaphore's own timeout.
        let sem3 = semaphores.clone();
        let third = acquire_model_permit(&sem3, "model-c", 1, 5, 1).await;
        assert!(
            matches!(third, Err(ProxyError::Overloaded(_, Some(_)))),
            "expected immediate rejection, got {:?}",
            third.map(|_| ())
        );

        // Release the held permit — the queued second call should now complete, and the
        // queue depth returns to steady-state 0.
        drop(first);
        let second = (&mut waiter).await.unwrap().unwrap();
        assert_eq!(second.queue_depth, 0);
    }

    #[test]
    fn test_suggested_retry_after_scales_with_queue_depth_and_caps() {
        assert_eq!(suggested_retry_after_secs(0), 5);
        assert_eq!(suggested_retry_after_secs(1), 7);
        assert_eq!(suggested_retry_after_secs(10), 25);
        // Caps at 60s even for very large backlogs.
        assert_eq!(suggested_retry_after_secs(1000), 60);
    }

    #[test]
    fn test_insert_ratelimit_headers_sets_both_headers() {
        let mut headers = axum::http::HeaderMap::new();
        insert_ratelimit_headers(&mut headers, 3, 7);
        assert_eq!(headers.get("x-ratelimit-remaining").unwrap(), "3");
        assert_eq!(headers.get("x-ratelimit-queue-depth").unwrap(), "7");
    }
}
