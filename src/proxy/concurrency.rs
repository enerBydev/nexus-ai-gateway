use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock as AsyncRwLock;
use tokio::sync::Semaphore;

use crate::circuit_breaker;
use crate::error::{ProxyError, ProxyResult};

/// Shared collection of per-model semaphores.
pub type ModelSemaphores = Arc<AsyncRwLock<HashMap<String, Arc<Semaphore>>>>;

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
pub(crate) async fn acquire_model_permit(
    semaphores: &ModelSemaphores,
    model: &str,
    max_concurrent: usize,
    permit_timeout: u64,
) -> ProxyResult<tokio::sync::OwnedSemaphorePermit> {
    let sem = {
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
                        "[GUARD] Created concurrency semaphore for '{}' ({} permits)",
                        model,
                        max_concurrent,
                    );
                    Arc::new(Semaphore::new(max_concurrent))
                })
                .clone()
        }
    };

    let available = sem.available_permits();
    if available == 0 {
        tracing::warn!(
            "⏳ Model '{}' at capacity (0/{} permits) — waiting up to {}s",
            model,
            max_concurrent,
            permit_timeout,
        );
    } else {
        tracing::debug!(
            "🎫 Acquiring permit for '{}' ({}/{} available)",
            model,
            available,
            max_concurrent,
        );
    }

    match tokio::time::timeout(
        std::time::Duration::from_secs(permit_timeout),
        sem.clone().acquire_owned(),
    )
    .await
    {
        Ok(Ok(permit)) => {
            tracing::debug!(
                "🎫 Permit acquired for '{}' ({}/{} remaining)",
                model,
                sem.available_permits(),
                max_concurrent,
            );
            Ok(permit)
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
            Err(ProxyError::Overloaded(format!(
                "Model '{}' concurrency limit reached ({} slots busy for {}s)",
                model, max_concurrent, permit_timeout,
            )))
        }
    }
}
