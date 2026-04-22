//! Proxy-side prompt caching for NIM self-hosted deployments.
//!
//! Tracks which content blocks marked with `cache_control` have been "seen"
//! recently, enabling accurate reporting of `cache_read_input_tokens` and
//! `cache_creation_input_tokens` to Claude Code.
//!
//! For NIM with NIM_ENABLE_KV_CACHE_REUSE=1, this allows CC to:
//! 1. See cache hit rates for context management
//! 2. Benefit from KV cache reuse for ~2x TTFT improvement
//! 3. Track costs appropriately

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

/// Location where cache_control was found in the request
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheLocation {
    SystemPrompt,
    MessageContent,
}

/// A single cache entry
#[derive(Debug, Clone)]
pub struct CacheEntry {
    /// SHA-256 hash of the cache_control-marked content
    pub content_hash: String,
    /// Estimated token count via tiktoken
    pub token_count: u32,
    /// When the cache entry was created
    pub created_at: Instant,
    /// Last access time for LRU eviction
    pub last_accessed: Instant,
    /// Time-to-live (matches Anthropic's ephemeral = 5 min)
    pub ttl: Duration,
    /// Where the cache marker was found
    pub location: CacheLocation,
}

/// Result of a cache lookup
#[derive(Debug, Clone)]
pub struct CacheHit {
    /// Number of tokens in the cached content
    pub token_count: u32,
    /// How long ago the cache entry was created
    pub age: Duration,
    /// Where the cache marker was found
    pub location: CacheLocation,
}

/// Cache statistics for monitoring
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CacheStats {
    pub total_entries: usize,
    pub total_hits: u64,
    pub total_misses: u64,
    pub total_evictions: u64,
    pub hit_rate: f64,
}

/// Proxy-side prompt cache for tracking content reuse
pub struct PromptCache {
    entries: RwLock<HashMap<String, CacheEntry>>,
    max_entries: usize,
    default_ttl: Duration,
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
}

impl PromptCache {
    /// Create a new prompt cache with specified capacity
    pub fn new(max_entries: usize, default_ttl: Duration) -> Self {
        Self {
            entries: RwLock::new(HashMap::with_capacity(max_entries.min(100))),
            max_entries,
            default_ttl,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
        }
    }

    /// Hash content using SHA-256
    pub fn hash_content(content: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// Estimate token count for content using tiktoken
    pub fn estimate_tokens(content: &str) -> u32 {
        // Use tiktoken cl100k_base singleton for ~95% accuracy
        let bpe = tiktoken_rs::cl100k_base_singleton();
        let tokens = bpe.encode_with_special_tokens(content);
        tokens.len().max(1) as u32
    }

    /// Lookup a content hash in the cache
    pub async fn lookup(&self, content_hash: &str) -> Option<CacheHit> {
        let mut entries = self.entries.write().await;
        if let Some(entry) = entries.get_mut(content_hash) {
            // Check if entry has expired
            if entry.created_at.elapsed() > entry.ttl {
                entries.remove(content_hash);
                self.evictions.fetch_add(1, Ordering::Relaxed);
                self.misses.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(
                    target: "nexus::cache",
                    "Cache EXPIRED: hash={}",
                    content_hash
                );
                return None;
            }

            // Update last accessed time
            entry.last_accessed = Instant::now();
            self.hits.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(
                target: "nexus::cache",
                "Cache HIT: hash={}, tokens={}, age={:?}",
                content_hash,
                entry.token_count,
                entry.created_at.elapsed()
            );
            Some(CacheHit {
                token_count: entry.token_count,
                age: entry.created_at.elapsed(),
                location: entry.location.clone(),
            })
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(
                target: "nexus::cache",
                "Cache MISS: hash={}",
                content_hash
            );
            None
        }
    }

    /// Store a new entry in the cache
    pub async fn store(
        &self,
        content_hash: &str,
        token_count: u32,
        location: CacheLocation,
    ) -> bool {
        let mut entries = self.entries.write().await;

        // LRU eviction if at capacity
        if entries.len() >= self.max_entries {
            if let Some(evict_key) = entries
                .iter()
                .min_by_key(|(_, e)| e.last_accessed)
                .map(|(k, _)| k.clone())
            {
                entries.remove(&evict_key);
                self.evictions.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(
                    target: "nexus::cache",
                    "Cache EVICT (LRU): hash={}",
                    evict_key
                );
            }
        }

        entries.insert(
            content_hash.to_string(),
            CacheEntry {
                content_hash: content_hash.to_string(),
                token_count,
                created_at: Instant::now(),
                last_accessed: Instant::now(),
                ttl: self.default_ttl,
                location,
            },
        );

        tracing::debug!(
            target: "nexus::cache",
            "Cache STORED: hash={}, tokens={}",
            content_hash,
            token_count
        );
        true
    }

    /// Evict expired entries, returns count of evicted entries
    pub async fn evict_expired(&self) -> usize {
        let mut entries = self.entries.write().await;
        let now = Instant::now();

        let expired: Vec<String> = entries
            .iter()
            .filter(|(_, e)| now.duration_since(e.created_at) > e.ttl)
            .map(|(k, _)| k.clone())
            .collect();

        let count = expired.len();
        for key in expired {
            entries.remove(&key);
        }

        self.evictions.fetch_add(count as u64, Ordering::Relaxed);
        if count > 0 {
            tracing::debug!(
                target: "nexus::cache",
                "Cache EVICT (expired): {} entries",
                count
            );
        }
        count
    }

    /// Get cache statistics
    pub async fn stats(&self) -> CacheStats {
        let entries = self.entries.read().await;
        let hits = self.hits.load(Ordering::Relaxed);
        let misses = self.misses.load(Ordering::Relaxed);
        let total = hits + misses;

        CacheStats {
            total_entries: entries.len(),
            total_hits: hits,
            total_misses: misses,
            total_evictions: self.evictions.load(Ordering::Relaxed),
            hit_rate: if total > 0 {
                hits as f64 / total as f64
            } else {
                0.0
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_content_deterministic() {
        let h1 = PromptCache::hash_content("hello world");
        let h2 = PromptCache::hash_content("hello world");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // SHA-256 hex = 64 chars
    }

    #[test]
    fn test_hash_content_different_inputs() {
        let h1 = PromptCache::hash_content("hello");
        let h2 = PromptCache::hash_content("world");
        assert_ne!(h1, h2);
    }

    #[tokio::test]
    async fn test_cache_store_and_lookup() {
        let cache = PromptCache::new(100, Duration::from_secs(300));
        let hash = PromptCache::hash_content("test content");

        cache.store(&hash, 100, CacheLocation::SystemPrompt).await;
        let hit = cache.lookup(&hash).await.unwrap();

        assert_eq!(hit.token_count, 100);
        assert_eq!(hit.location, CacheLocation::SystemPrompt);
    }

    #[tokio::test]
    async fn test_cache_miss_returns_none() {
        let cache = PromptCache::new(100, Duration::from_secs(300));
        let result = cache.lookup("nonexistent").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_cache_ttl_expiration() {
        let cache = PromptCache::new(100, Duration::from_millis(10));
        let hash = PromptCache::hash_content("expiring content");

        cache.store(&hash, 50, CacheLocation::MessageContent).await;

        // Wait for TTL to expire
        tokio::time::sleep(Duration::from_millis(50)).await;

        let result = cache.lookup(&hash).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_cache_lru_eviction() {
        let cache = PromptCache::new(2, Duration::from_secs(300));
        let h1 = PromptCache::hash_content("content1");
        let h2 = PromptCache::hash_content("content2");
        let h3 = PromptCache::hash_content("content3");

        cache.store(&h1, 10, CacheLocation::SystemPrompt).await;
        cache.store(&h2, 20, CacheLocation::SystemPrompt).await;

        // This should evict h1 (LRU)
        cache.store(&h3, 30, CacheLocation::MessageContent).await;

        assert!(cache.lookup(&h1).await.is_none());
        assert!(cache.lookup(&h2).await.is_some());
        assert!(cache.lookup(&h3).await.is_some());
    }

    #[tokio::test]
    async fn test_cache_stats_accuracy() {
        let cache = PromptCache::new(100, Duration::from_secs(300));
        let hash = PromptCache::hash_content("stats test");

        cache.store(&hash, 10, CacheLocation::SystemPrompt).await;
        cache.lookup(&hash).await; // hit
        cache.lookup("nonexistent").await; // miss

        let stats = cache.stats().await;
        assert_eq!(stats.total_entries, 1);
        assert_eq!(stats.total_hits, 1);
        assert_eq!(stats.total_misses, 1);
        assert!((stats.hit_rate - 0.5).abs() < 0.01);
    }
}
