//! Circuit Breaker for upstream request protection
//!
//! Implements the circuit breaker pattern to prevent cascade failures
//! when the upstream API becomes unhealthy.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Circuit breaker states
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CircuitState {
    /// Normal operation - requests are allowed
    Closed,
    /// Failing fast - requests are rejected
    Open,
    /// Testing if upstream has recovered
    HalfOpen,
}

/// Circuit breaker for upstream requests
///
/// Tracks consecutive failures and opens the circuit when threshold is reached.
/// After recovery timeout, transitions to half-open to test if upstream recovered.
/// Uses CAS-based probe admission to guarantee only one probe at a time.
/// Uses generation counter to prevent stale in-flight completions from changing state.
pub struct CircuitBreaker {
    /// When false, the circuit breaker is a no-op (always allows, never records).
    enabled: bool,
    state: RwLock<CircuitState>,
    failure_count: AtomicU32,
    failure_threshold: u32,
    recovery_timeout: Duration,
    last_failure: RwLock<Option<Instant>>,
    /// Tracks how many probes have been admitted in current HalfOpen phase.
    /// CAS on this field ensures only one probe is admitted at a time.
    half_open_probes: AtomicU32,
    /// Generation counter incremented on each HalfOpen transition.
    /// Callers receive the generation when admitted and must present it
    /// when recording results, preventing stale in-flight completions.
    generation: AtomicU32,
}

impl CircuitBreaker {
    pub fn new(failure_threshold: u32, recovery_timeout: Duration) -> Self {
        Self {
            enabled: true,
            state: RwLock::new(CircuitState::Closed),
            failure_count: AtomicU32::new(0),
            failure_threshold,
            recovery_timeout,
            last_failure: RwLock::new(None),
            half_open_probes: AtomicU32::new(0),
            generation: AtomicU32::new(0),
        }
    }

    /// Create a disabled circuit breaker (no-op: always allows, never records).
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            state: RwLock::new(CircuitState::Closed),
            failure_count: AtomicU32::new(0),
            failure_threshold: 0,
            recovery_timeout: Duration::from_secs(0),
            last_failure: RwLock::new(None),
            half_open_probes: AtomicU32::new(0),
            generation: AtomicU32::new(0),
        }
    }

    /// Check if requests should be allowed.
    /// Returns (allowed, generation) where generation must be passed
    /// to record_success/record_failure to prevent stale completions.
    pub async fn is_allowed(&self) -> (bool, u32) {
        if !self.enabled {
            return (true, 0);
        }
        let state = *self.state.read().await;
        match state {
            CircuitState::Closed => {
                let gen = self.generation.load(Ordering::SeqCst);
                (true, gen)
            }
            CircuitState::Open => {
                let last = self.last_failure.read().await;
                if let Some(instant) = *last {
                    if instant.elapsed() >= self.recovery_timeout {
                        // Atomically reserve the probe slot via CAS.
                        // This ensures only one caller gets through the transition.
                        if self
                            .half_open_probes
                            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
                            .is_err()
                        {
                            // Another thread already transitioned and reserved the probe
                            drop(last);
                            tracing::debug!(
                                "HalfOpen: probe already reserved by another thread during transition"
                            );
                            return (false, 0);
                        }
                        // Increment generation to invalidate stale in-flight completions
                        let new_gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
                        drop(last);
                        *self.state.write().await = CircuitState::HalfOpen;
                        tracing::info!(
                            "Circuit breaker transitioning to HALF-OPEN (generation={})",
                            new_gen
                        );
                        return (true, new_gen);
                    }
                }
                (false, 0)
            }
            CircuitState::HalfOpen => {
                // CAS ensures only one probe is admitted at a time.
                // Unlike fetch_add, rejected callers don't increment the counter.
                if self
                    .half_open_probes
                    .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    let gen = self.generation.load(Ordering::SeqCst);
                    tracing::debug!("HalfOpen: allowing probe request (generation={})", gen);
                    (true, gen)
                } else {
                    tracing::debug!("HalfOpen: rejecting request (probe already in progress)");
                    (false, 0)
                }
            }
        }
    }

    /// Get current state
    #[allow(dead_code)]
    pub async fn state(&self) -> CircuitState {
        *self.state.read().await
    }

    /// Record a successful request.
    /// The generation parameter must match the current generation to prevent
    /// stale in-flight completions from closing the circuit.
    pub async fn record_success(&self, generation: u32) {
        if !self.enabled {
            return;
        }
        let current_state = *self.state.read().await;
        let current_gen = self.generation.load(Ordering::SeqCst);

        // Reject ALL stale completions regardless of current state.
        // A generation mismatch means the result is from a previous
        // HalfOpen phase — even if the circuit has since closed,
        // stale results must not mutate the breaker.
        if generation != current_gen {
            tracing::warn!(
                "Ignoring stale success (gen={}, current_gen={}) - result from previous generation",
                generation,
                current_gen
            );
            return;
        }

        match current_state {
            CircuitState::HalfOpen => {
                tracing::info!(
                    "Circuit breaker closing after successful probe (generation={})",
                    generation
                );
                self.failure_count.store(0, Ordering::SeqCst);
                self.half_open_probes.store(0, Ordering::SeqCst);
                *self.state.write().await = CircuitState::Closed;
            }
            CircuitState::Closed => {
                // Normal operation - just reset failure count
                self.failure_count.store(0, Ordering::SeqCst);
            }
            CircuitState::Open => {
                tracing::warn!("Received success while circuit is OPEN - ignoring stale result");
            }
        }
    }

    /// Record a failed request.
    /// The generation parameter must match the current generation to prevent
    /// stale in-flight completions from re-opening the circuit.
    pub async fn record_failure(&self, generation: u32) {
        if !self.enabled {
            return;
        }
        let current_gen = self.generation.load(Ordering::SeqCst);
        let current_state = *self.state.read().await;

        // Reject ALL stale completions regardless of current state.
        // A generation mismatch means the result is from a previous
        // HalfOpen phase — stale results must not mutate the breaker.
        if generation != current_gen {
            tracing::debug!(
                "Ignoring stale failure (gen={}, current_gen={}) - result from previous generation",
                generation,
                current_gen
            );
            return;
        }

        let count = self.failure_count.fetch_add(1, Ordering::SeqCst) + 1;
        *self.last_failure.write().await = Some(Instant::now());

        if current_state == CircuitState::HalfOpen {
            // Failed during half-open, go back to open
            self.half_open_probes.store(0, Ordering::SeqCst);
            *self.state.write().await = CircuitState::Open;
            tracing::warn!(
                "Circuit breaker back to OPEN after failure in half-open (generation={})",
                generation
            );
        } else if count >= self.failure_threshold {
            *self.state.write().await = CircuitState::Open;
            tracing::warn!(
                "Circuit breaker OPEN after {} consecutive failures (threshold={})",
                count,
                self.failure_threshold
            );
        }
    }
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new(3, Duration::from_secs(30))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_circuit_breaker_starts_closed() {
        let cb = CircuitBreaker::default();
        let (allowed, _gen) = cb.is_allowed().await;
        assert!(allowed);
        assert_eq!(cb.state().await, CircuitState::Closed);
    }

    #[tokio::test]
    async fn test_circuit_breaker_opens_after_threshold() {
        let cb = CircuitBreaker::new(2, Duration::from_secs(30));
        let (allowed, gen) = cb.is_allowed().await;
        assert!(allowed);
        cb.record_failure(gen).await;
        assert_eq!(cb.state().await, CircuitState::Closed);

        let (allowed, gen) = cb.is_allowed().await;
        assert!(allowed);
        cb.record_failure(gen).await;
        assert_eq!(cb.state().await, CircuitState::Open);

        let (allowed, _) = cb.is_allowed().await;
        assert!(!allowed);
    }

    #[tokio::test]
    async fn test_circuit_breaker_resets_on_success() {
        let cb = CircuitBreaker::new(2, Duration::from_secs(30));
        let (allowed, gen) = cb.is_allowed().await;
        assert!(allowed);
        cb.record_failure(gen).await;

        let (allowed, gen) = cb.is_allowed().await;
        assert!(allowed);
        cb.record_success(gen).await;
        assert_eq!(cb.state().await, CircuitState::Closed);

        let (allowed, _) = cb.is_allowed().await;
        assert!(allowed);
    }

    #[tokio::test]
    async fn test_stale_success_ignored() {
        let cb = CircuitBreaker::new(2, Duration::from_millis(0));

        // Get admission with generation 0
        let (allowed, old_gen) = cb.is_allowed().await;
        assert!(allowed);
        assert_eq!(old_gen, 0);

        // Open the circuit
        cb.record_failure(old_gen).await;
        cb.record_failure(old_gen).await;
        assert_eq!(cb.state().await, CircuitState::Open);

        // Move to half-open, which advances the generation.
        // Recovery timeout is 0ms so the transition is immediate.
        let (allowed, new_gen) = cb.is_allowed().await;
        assert!(allowed);
        assert_eq!(new_gen, 1);
        assert_eq!(cb.state().await, CircuitState::HalfOpen);

        // A stale success with old generation should be ignored.
        cb.record_success(old_gen).await;
        assert_eq!(cb.state().await, CircuitState::HalfOpen);

        // A current-generation success should close the circuit.
        cb.record_success(new_gen).await;
        assert_eq!(cb.state().await, CircuitState::Closed);
    }
}
