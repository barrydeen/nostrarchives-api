use std::sync::Arc;

use redis::AsyncCommands;

use crate::db::models::{GlobalStats, KindCount};
use crate::db::repository::EventRepository;
use crate::error::AppError;
use crate::live_metrics::LiveMetricsTracker;

const PREFIX: &str = "nostr";
const STATS_TTL: u64 = 60; // seconds
const SEARCH_SUGGEST_TTL: u64 = 60; // seconds

/// TTL for trending/top-notes cache, keyed by range.
fn trending_ttl(range: &str) -> u64 {
    match range {
        "today" => 300, // 5 min — balanced freshness vs DB load
        "7d" => 300,   // 5 min
        "30d" => 900,  // 15 min
        "1y" => 1800,  // 30 min
        "all" => 3600, // 1 hour — rarely changes
        _ => 120,
    }
}

fn key(suffix: &str) -> String {
    format!("{PREFIX}:{suffix}")
}

#[derive(Clone)]
pub struct StatsCache {
    redis: redis::Client,
    repo: EventRepository,
    live_tracker: Option<Arc<LiveMetricsTracker>>,
}

impl StatsCache {
    pub fn new(redis: redis::Client, repo: EventRepository) -> Self {
        Self { redis, repo, live_tracker: None }
    }

    /// Attach a live metrics tracker for real-time event streaming.
    pub fn set_live_tracker(&mut self, tracker: Arc<LiveMetricsTracker>) {
        self.live_tracker = Some(tracker);
    }

    /// Record a newly ingested event in Redis counters.
    ///
    /// Tracks global stats (existing) plus daily DAU (HyperLogLog) and daily
    /// posts (kind-1 counter). Daily zap sats are tracked separately via
    /// `record_daily_zap_sats` from the zap metadata extraction path.
    pub async fn on_event_ingested(&self, pubkey: &str, kind: i64) {
        let Ok(mut conn) = self.redis.get_multiplexed_async_connection().await else {
            return;
        };

        // Today's date key (UTC) for daily stats — keys auto-expire after 48h.
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let dau_key = key(&format!("daily:dau:{today}"));
        let posts_key = key(&format!("daily:posts:{today}"));

        let mut pipe = redis::pipe();
        pipe.atomic()
            // Global counters (existing)
            .cmd("INCR").arg(key("total_events"))
            .cmd("PFADD").arg(key("unique_pubkeys")).arg(pubkey)
            .cmd("HINCRBY").arg(key("events_by_kind")).arg(kind.to_string()).arg(1i64)
            .cmd("INCR").arg(key("ingestion_window"))
            // Daily DAU via HyperLogLog
            .cmd("PFADD").arg(&dau_key).arg(pubkey);

        // Daily posts counter (kind 1 only)
        if kind == 1 {
            pipe.cmd("INCR").arg(&posts_key);
        }

        let _: Result<(), _> = pipe.query_async(&mut conn).await;

        // Set TTLs — ingestion window resets every 60s, daily keys expire after 48h
        let _: Result<(), _> = redis::pipe()
            .cmd("EXPIRE").arg(key("ingestion_window")).arg(60i64)
            .cmd("EXPIRE").arg(&dau_key).arg(172_800i64)
            .cmd("EXPIRE").arg(&posts_key).arg(172_800i64)
            .query_async(&mut conn)
            .await;

        // Forward to live metrics tracker (online + notes only; zap sats handled separately)
        if let Some(ref tracker) = self.live_tracker {
            tracker.record_event(pubkey, kind, 0).await;
        }
    }

    /// Record zap sats in the live metrics tracker (called from the ingester
    /// after extracting the zap amount from kind-9735 events).
    pub async fn record_live_zap(&self, zap_sats: i64) {
        if zap_sats <= 0 {
            return;
        }
        if let Some(ref tracker) = self.live_tracker {
            tracker.record_zap_sats(zap_sats).await;
        }
    }

    /// Get daily DAU and posts from Redis HLL + counters.
    ///
    /// Returns `None` if Redis keys don't exist yet (cold start / restart).
    /// Zap sats are NOT included here — they're queried from the DB using the
    /// indexed `zap_metadata.created_at` column (5ms with index).
    pub async fn get_daily_dau_posts(&self) -> Option<(i64, i64)> {
        let Ok(mut conn) = self.redis.get_multiplexed_async_connection().await else {
            return None;
        };

        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let dau_key = key(&format!("daily:dau:{today}"));
        let posts_key = key(&format!("daily:posts:{today}"));

        // If the DAU key doesn't exist, this is a cold start.
        let exists: bool = redis::cmd("EXISTS")
            .arg(&dau_key)
            .query_async(&mut conn)
            .await
            .unwrap_or(false);
        if !exists {
            return None;
        }

        let dau: i64 = redis::cmd("PFCOUNT")
            .arg(&dau_key)
            .query_async(&mut conn)
            .await
            .unwrap_or(0);

        let daily_posts: i64 = conn.get(&posts_key).await.unwrap_or(0);

        Some((dau, daily_posts))
    }

    /// Get global stats, preferring cached values, falling back to DB.
    pub async fn get_stats(&self) -> Result<GlobalStats, AppError> {
        match self.get_cached_stats().await {
            Ok(stats) => Ok(stats),
            Err(_) => self.refresh_stats().await,
        }
    }

    async fn get_cached_stats(&self) -> Result<GlobalStats, AppError> {
        let mut conn = self.redis.get_multiplexed_async_connection().await?;

        let total_events: Option<i64> = conn.get(&key("total_events")).await.ok();
        let unique_pubkeys: Option<i64> = redis::cmd("PFCOUNT")
            .arg(&key("unique_pubkeys"))
            .query_async(&mut conn)
            .await
            .ok();
        let ingestion_count: Option<i64> = conn.get(&key("ingestion_window")).await.ok();

        let total_events = total_events.ok_or_else(|| AppError::Internal("cache miss".into()))?;
        let unique_pubkeys = unique_pubkeys.unwrap_or(0);
        let ingestion_rate_per_min = ingestion_count.unwrap_or(0) as f64;

        // Events by kind from Redis hash
        let kind_map: std::collections::HashMap<String, i64> = conn
            .hgetall(&key("events_by_kind"))
            .await
            .unwrap_or_default();

        let mut events_by_kind: Vec<KindCount> = kind_map
            .into_iter()
            .filter_map(|(k, count)| k.parse::<i32>().ok().map(|kind| KindCount { kind, count }))
            .collect();
        events_by_kind.sort_by(|a, b| b.count.cmp(&a.count));
        events_by_kind.truncate(20);

        Ok(GlobalStats {
            total_events,
            unique_pubkeys,
            events_by_kind,
            ingestion_rate_per_min,
        })
    }

    /// Get cached search suggestions for a query.
    pub async fn get_search_suggest(&self, query: &str) -> Option<String> {
        let Ok(mut conn) = self.redis.get_multiplexed_async_connection().await else {
            return None;
        };
        let cache_key = key(&format!("search:suggest:{}", query.to_lowercase()));
        conn.get(&cache_key).await.ok()
    }

    /// Cache search suggestions for a query.
    pub async fn set_search_suggest(&self, query: &str, json: &str) {
        let Ok(mut conn) = self.redis.get_multiplexed_async_connection().await else {
            return;
        };
        let cache_key = key(&format!("search:suggest:{}", query.to_lowercase()));
        let _: Result<(), _> = conn.set_ex(&cache_key, json, SEARCH_SUGGEST_TTL).await;
    }

    /// Get cached trending results for a metric+range+limit+offset combo.
    pub async fn get_trending(
        &self,
        metric: &str,
        range: &str,
        limit: i64,
        offset: i64,
    ) -> Option<String> {
        let Ok(mut conn) = self.redis.get_multiplexed_async_connection().await else {
            return None;
        };
        let cache_key = key(&format!("trending:{metric}:{range}:{limit}:{offset}"));
        conn.get(&cache_key).await.ok()
    }

    /// Cache trending results.
    pub async fn set_trending(
        &self,
        metric: &str,
        range: &str,
        limit: i64,
        offset: i64,
        json: &str,
    ) {
        let Ok(mut conn) = self.redis.get_multiplexed_async_connection().await else {
            return;
        };
        let cache_key = key(&format!("trending:{metric}:{range}:{limit}:{offset}"));
        let ttl = trending_ttl(range);
        let _: Result<(), _> = conn.set_ex(&cache_key, json, ttl).await;
    }

    /// Generic JSON cache: get
    pub async fn get_json(&self, cache_key: &str) -> Option<String> {
        let Ok(mut conn) = self.redis.get_multiplexed_async_connection().await else {
            return None;
        };
        conn.get(&key(cache_key)).await.ok()
    }

    /// Generic JSON cache: delete
    pub async fn delete_json(&self, cache_key: &str) {
        let Ok(mut conn) = self.redis.get_multiplexed_async_connection().await else {
            return;
        };
        let _: Result<(), _> = conn.del(&key(cache_key)).await;
    }

    /// Delete all keys whose cache_key starts with `prefix` (e.g. "analytics:top_posters").
    /// Uses SCAN to avoid blocking Redis.
    pub async fn delete_by_prefix(&self, prefix: &str) {
        let Ok(mut conn) = self.redis.get_multiplexed_async_connection().await else {
            return;
        };
        let pattern = format!("{}:*", key(prefix));
        let mut cursor: u64 = 0;
        loop {
            let (next_cursor, keys): (u64, Vec<String>) =
                match redis::cmd("SCAN")
                    .arg(cursor)
                    .arg("MATCH")
                    .arg(&pattern)
                    .arg("COUNT")
                    .arg(100u64)
                    .query_async(&mut conn)
                    .await
                {
                    Ok(result) => result,
                    Err(_) => break,
                };
            if !keys.is_empty() {
                let _: Result<(), _> = conn.del(keys).await;
            }
            cursor = next_cursor;
            if cursor == 0 {
                break;
            }
        }
    }

    /// Generic JSON cache: set with TTL
    pub async fn set_json(&self, cache_key: &str, json: &str, ttl: u64) {
        let Ok(mut conn) = self.redis.get_multiplexed_async_connection().await else {
            return;
        };
        let _: Result<(), _> = conn.set_ex(&key(cache_key), json, ttl).await;
    }

    /// Refresh stats from the database and populate cache.
    pub async fn refresh_stats(&self) -> Result<GlobalStats, AppError> {
        let total_events = self.repo.count_events().await?;
        let unique_pubkeys = self.repo.count_unique_pubkeys().await?;
        let events_by_kind = self.repo.events_by_kind().await?;

        // Populate Redis cache
        if let Ok(mut conn) = self.redis.get_multiplexed_async_connection().await {
            let _: Result<(), _> = conn
                .set_ex(&key("total_events"), total_events, STATS_TTL)
                .await;
            // Re-populate kind hash
            for kc in &events_by_kind {
                let _: Result<(), _> = conn
                    .hset::<_, _, _, ()>(&key("events_by_kind"), kc.kind.to_string(), kc.count)
                    .await;
            }
        }

        Ok(GlobalStats {
            total_events,
            unique_pubkeys,
            events_by_kind,
            ingestion_rate_per_min: 0.0,
        })
    }

    /// Delete all Redis keys matching a prefix pattern.
    /// Used to invalidate cached data after admin moderation actions.
    pub async fn invalidate_pattern(&self, pattern: &str) {
        let Ok(mut conn) = self.redis.get_multiplexed_async_connection().await else {
            return;
        };
        let full_pattern = key(&format!("{pattern}*"));
        let keys: Vec<String> = match redis::cmd("KEYS")
            .arg(&full_pattern)
            .query_async(&mut conn)
            .await
        {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!(error = %e, pattern, "Failed to scan keys for invalidation");
                return;
            }
        };
        if keys.is_empty() {
            return;
        }
        let count = keys.len();
        let _: Result<(), _> = redis::cmd("DEL")
            .arg(&keys)
            .query_async(&mut conn)
            .await;
        tracing::info!(pattern, count, "Invalidated cached keys");
    }
}
