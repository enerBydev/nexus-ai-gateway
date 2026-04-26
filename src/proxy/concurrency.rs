use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock as AsyncRwLock;
use tokio::sync::Semaphore;

use crate::circuit_breaker;
use crate::error::{ProxyError, ProxyResult};

/// Shared collection of per-model semaphores.
pub type ModelSemaphores = Arc<AsyncRwLock<HashMap<String, Arc<Semaphore>>>>;
pub type CircuitBreaker = Arc<circuit_breaker::CircuitBreaker>;

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
                        "🛡️ Created concurrency semaphore for '{}' ({} permits)",
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
            tracing::error!("🛡️ Semaphore CLOSED for '{}' — this is a bug", model);
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
