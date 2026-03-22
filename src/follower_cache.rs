use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use sqlx::PgPool;

use crate::error::AppError;

/// High-performance follower threshold cache to avoid database lookups on every event.
/// Loads qualified pubkeys into a HashSet for O(1) lookups during ingestion.
#[derive(Clone)]
pub struct FollowerCache {
    /// Set of pubkeys that meet the minimum follower threshold
    qualified_pubkeys: Arc<RwLock<HashSet<String>>>,
    /// When the cache was last refreshed
    last_refresh: Arc<RwLock<Instant>>,
    /// How often to refresh the cache
    refresh_interval: Duration,
    /// Minimum follower threshold
    threshold: i64,
    /// Database connection pool
    pool: PgPool,
}

impl FollowerCache {
    /// Create a new follower cache with the given threshold and refresh interval.
    pub fn new(pool: PgPool, threshold: i64, refresh_interval_secs: u64) -> Self {
        Self {
            qualified_pubkeys: Arc::new(RwLock::new(HashSet::new())),
            last_refresh: Arc::new(RwLock::new(Instant::now() - Duration::from_secs(refresh_interval_secs + 1))),
            refresh_interval: Duration::from_secs(refresh_interval_secs),
            threshold,
            pool,
        }
    }

    /// Initialize the cache by loading qualified pubkeys from the database.
    pub async fn initialize(&self) -> Result<(), AppError> {
        self.refresh_cache().await
    }

    /// Check if a pubkey meets the follower threshold.
    /// Returns true for unknown pubkeys (new users) to avoid blocking them.
    pub async fn meets_threshold(&self, pubkey: &str) -> Result<bool, AppError> {
        // Check if cache needs refresh
        {
            let last_refresh = *self.last_refresh.read().await;
            if last_refresh.elapsed() > self.refresh_interval {
                // Try to refresh, but don't block if it fails
                if let Err(e) = self.refresh_cache().await {
                    tracing::warn!(error = %e, "Failed to refresh follower cache");
                }
            }
        }

        // Check the cache
        let qualified = self.qualified_pubkeys.read().await;
        
        // If threshold is 0, everyone qualifies
        if self.threshold == 0 {
            return Ok(true);
        }
        
        // If the pubkey is in our qualified set, they meet the threshold
        if qualified.contains(pubkey) {
            return Ok(true);
        }

        // Unknown pubkeys don't meet the threshold
        Ok(false)
    }

    /// Refresh the cache from the database.
    async fn refresh_cache(&self) -> Result<(), AppError> {
        let start = Instant::now();
        
        tracing::debug!(threshold = self.threshold, "Refreshing follower cache");

        // Load all qualified pubkeys from profile_search
        let qualified_pubkeys: Vec<String> = sqlx::query_scalar(
            "SELECT pubkey FROM profile_search WHERE follower_count >= $1"
        )
        .bind(self.threshold)
        .fetch_all(&self.pool)
        .await?;

        let count = qualified_pubkeys.len();

        // Update the cache
        {
            let mut cache = self.qualified_pubkeys.write().await;
            cache.clear();
            cache.extend(qualified_pubkeys);
        }

        // Update refresh timestamp
        {
            let mut last_refresh = self.last_refresh.write().await;
            *last_refresh = Instant::now();
        }

        let elapsed = start.elapsed();
        tracing::info!(
            count = count,
            threshold = self.threshold, 
            elapsed_ms = elapsed.as_millis(),
            "Follower cache refreshed"
        );

        Ok(())
    }

    /// Get cache statistics for monitoring.
    pub async fn stats(&self) -> CacheStats {
        let qualified_count = self.qualified_pubkeys.read().await.len();
        let last_refresh = *self.last_refresh.read().await;
        
        CacheStats {
            qualified_count,
            threshold: self.threshold,
            last_refresh_ago: last_refresh.elapsed(),
            refresh_interval: self.refresh_interval,
        }
    }
}

#[derive(Debug)]
pub struct CacheStats {
    pub qualified_count: usize,
    pub threshold: i64,
    pub last_refresh_ago: Duration,
    pub refresh_interval: Duration,
}