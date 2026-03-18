use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use sqlx::PgPool;

use crate::error::AppError;

/// Two-level Web of Trust cache.
///
/// A pubkey passes WoT if it has 21+ followers, where each of those
/// followers also has 21+ followers. Unknown pubkeys are **rejected**
/// for kind-1 notes (fundamental change from v1's FollowerCache).
///
/// Kind 0/3/10002/9735 events always bypass WoT (needed for graph
/// building and zap tracking).
#[derive(Clone)]
pub struct WotCache {
    /// Set of pubkeys that pass the two-level WoT check
    passing_pubkeys: Arc<RwLock<HashSet<String>>>,
    /// When the cache was last refreshed
    last_refresh: Arc<RwLock<Instant>>,
    /// How often to refresh the cache
    refresh_interval: Duration,
    /// Minimum followers at each level
    threshold: i64,
    /// Database connection pool
    pool: PgPool,
}

impl WotCache {
    pub fn new(pool: PgPool, threshold: i64, refresh_interval_secs: u64) -> Self {
        Self {
            passing_pubkeys: Arc::new(RwLock::new(HashSet::new())),
            last_refresh: Arc::new(RwLock::new(
                Instant::now() - Duration::from_secs(refresh_interval_secs + 1),
            )),
            refresh_interval: Duration::from_secs(refresh_interval_secs),
            threshold,
            pool,
        }
    }

    /// Initialize the cache by loading qualifying pubkeys from the database.
    pub async fn initialize(&self) -> Result<(), AppError> {
        self.refresh_cache().await
    }

    /// Check if a pubkey passes the two-level WoT check.
    /// Returns false for unknown pubkeys (rejects spam).
    pub async fn passes_wot(&self, pubkey: &str) -> Result<bool, AppError> {
        // Trigger refresh if stale
        {
            let last_refresh = *self.last_refresh.read().await;
            if last_refresh.elapsed() > self.refresh_interval {
                if let Err(e) = self.refresh_cache().await {
                    tracing::warn!(error = %e, "Failed to refresh WoT cache");
                }
            }
        }

        if self.threshold == 0 {
            return Ok(true);
        }

        let passing = self.passing_pubkeys.read().await;
        Ok(passing.contains(pubkey))
    }

    /// Refresh the cache from the database using a two-level follower query.
    /// Also persists results to wot_scores table for crawler prioritization.
    async fn refresh_cache(&self) -> Result<(), AppError> {
        let start = Instant::now();

        tracing::debug!(threshold = self.threshold, "Refreshing WoT cache");

        // Two-level WoT query:
        // Level 1: pubkeys with threshold+ followers
        // Level 2: of those followers, how many themselves have threshold+ followers?
        // A pubkey passes if it has threshold+ "quality" followers (level-1 pubkeys following them)
        let rows: Vec<(String, i64, i64)> = sqlx::query_as(
            r#"
            WITH level1 AS (
                SELECT followed_pubkey AS pubkey, COUNT(*) AS cnt
                FROM follows
                GROUP BY followed_pubkey
                HAVING COUNT(*) >= $1
            )
            SELECT f.followed_pubkey AS pubkey,
                   COUNT(*)::bigint AS quality_followers,
                   (SELECT COALESCE(cnt, 0) FROM level1 WHERE pubkey = f.followed_pubkey)::bigint AS total_followers
            FROM follows f
            INNER JOIN level1 l ON l.pubkey = f.follower_pubkey
            GROUP BY f.followed_pubkey
            HAVING COUNT(*) >= $1
            "#,
        )
        .bind(self.threshold)
        .fetch_all(&self.pool)
        .await?;

        let count = rows.len();
        let mut new_set = HashSet::with_capacity(count);

        // Persist to wot_scores table (truncate + bulk insert for efficiency)
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM wot_scores")
            .execute(&mut *tx)
            .await?;

        for (pubkey, quality_followers, total_followers) in &rows {
            new_set.insert(pubkey.clone());
            sqlx::query(
                "INSERT INTO wot_scores (pubkey, follower_count, quality_followers, passes_wot, computed_at)
                 VALUES ($1, $2, $3, true, NOW())"
            )
            .bind(pubkey)
            .bind(*total_followers as i32)
            .bind(*quality_followers as i32)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;

        // Update in-memory cache
        {
            let mut cache = self.passing_pubkeys.write().await;
            *cache = new_set;
        }

        {
            let mut last_refresh = self.last_refresh.write().await;
            *last_refresh = Instant::now();
        }

        let elapsed = start.elapsed();
        tracing::info!(
            count = count,
            threshold = self.threshold,
            elapsed_ms = elapsed.as_millis(),
            "WoT cache refreshed"
        );

        Ok(())
    }

    /// Get cache statistics for monitoring.
    pub async fn stats(&self) -> WotCacheStats {
        let passing_count = self.passing_pubkeys.read().await.len();
        let last_refresh = *self.last_refresh.read().await;

        WotCacheStats {
            passing_count,
            threshold: self.threshold,
            last_refresh_ago: last_refresh.elapsed(),
            refresh_interval: self.refresh_interval,
        }
    }
}

#[derive(Debug)]
pub struct WotCacheStats {
    pub passing_count: usize,
    pub threshold: i64,
    pub last_refresh_ago: Duration,
    pub refresh_interval: Duration,
}
