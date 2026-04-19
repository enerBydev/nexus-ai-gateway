//! Circuit Breaker for upstream request protection
//!
//! Implements the circuit breaker pattern to prevent cascade failures
//! when the upstream API becomes unhealthy.

#![allow(dead_code)] // v0.12.0: Circuit breaker integration in progress

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
pub struct CircuitBreaker {
    state: RwLock<CircuitState>,
    failure_count: AtomicU32,
    failure_threshold: u32,
    recovery_timeout: Duration,
    last_failure: RwLock<Option<Instant>>,
}

impl CircuitBreaker {
    /// Create a new circuit breaker
    ///
    /// # Arguments
    /// * `failure_threshold` - Number of consecutive failures before opening
    /// * `recovery_timeout` - Time to wait before testing recovery
    pub fn new(failure_threshold: u32, recovery_timeout: Duration) -> Self {
        Self {
            state: RwLock::new(CircuitState::Closed),
            failure_count: AtomicU32::new(0),
            failure_threshold,
            recovery_timeout,
            last_failure: RwLock::new(None),
        }
    }

    /// Check if requests should be allowed
    pub async fn is_allowed(&self) -> bool {
        let state = *self.state.read().await;
        match state {
            CircuitState::Closed => true,
            CircuitState::Open => {
                // Check if recovery timeout has passed
                let last = self.last_failure.read().await;
                if let Some(instant) = *last {
                    if instant.elapsed() >= self.recovery_timeout {
                        // Transition to half-open
                        drop(last);
                        *self.state.write().await = CircuitState::HalfOpen;
                        tracing::info!("🟡 Circuit breaker transitioning to HALF-OPEN");
                        return true;
                    }
                }
                false
            }
            CircuitState::HalfOpen => true,
        }
    }

    /// Get current state
    pub async fn state(&self) -> CircuitState {
        *self.state.read().await
    }

    /// Record a successful request
    pub async fn record_success(&self) {
        let current_state = *self.state.read().await;
        if current_state == CircuitState::HalfOpen {
            tracing::info!("🟢 Circuit breaker closing after successful request");
        }
        self.failure_count.store(0, Ordering::SeqCst);
        *self.state.write().await = CircuitState::Closed;
    }

    /// Record a failed request
    pub async fn record_failure(&self) {
        let count = self.failure_count.fetch_add(1, Ordering::SeqCst) + 1;
        *self.last_failure.write().await = Some(Instant::now());
        let current_state = *self.state.read().await;
        if current_state == CircuitState::HalfOpen {
            // Failed during half-open, go back to open
            *self.state.write().await = CircuitState::Open;
            tracing::warn!("🔴 Circuit breaker back to OPEN after failure in half-open");
        } else if count >= self.failure_threshold {
            *self.state.write().await = CircuitState::Open;
            tracing::warn!(
                "🔴 Circuit breaker OPEN after {} consecutive failures (threshold={})",
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
        assert!(cb.is_allowed().await);
        assert_eq!(cb.state().await, CircuitState::Closed);
    }

    #[tokio::test]
    async fn test_circuit_breaker_opens_after_threshold() {
        let cb = CircuitBreaker::new(2, Duration::from_secs(30));
        cb.record_failure().await;
        assert_eq!(cb.state().await, CircuitState::Closed);
        cb.record_failure().await;
        assert_eq!(cb.state().await, CircuitState::Open);
        assert!(!cb.is_allowed().await);
    }

    #[tokio::test]
    async fn test_circuit_breaker_resets_on_success() {
        let cb = CircuitBreaker::new(2, Duration::from_secs(30));
        cb.record_failure().await;
        cb.record_success().await;
        assert_eq!(cb.state().await, CircuitState::Closed);
        assert!(cb.is_allowed().await);
    }
}
