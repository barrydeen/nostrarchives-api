use redis::AsyncCommands;

use crate::db::models::{GlobalStats, KindCount};
use crate::db::repository::EventRepository;
use crate::error::AppError;

const PREFIX: &str = "nostr";
const STATS_TTL: u64 = 60; // seconds

fn key(suffix: &str) -> String {
    format!("{PREFIX}:{suffix}")
}

#[derive(Clone)]
pub struct StatsCache {
    redis: redis::Client,
    repo: EventRepository,
}

impl StatsCache {
    pub fn new(redis: redis::Client, repo: EventRepository) -> Self {
        Self { redis, repo }
    }

    /// Record a newly ingested event in Redis counters.
    pub async fn on_event_ingested(&self, pubkey: &str, kind: i64) {
        let Ok(mut conn) = self.redis.get_multiplexed_async_connection().await else {
            return;
        };
        let _: Result<(), _> = redis::pipe()
            .atomic()
            .cmd("INCR")
            .arg(key("total_events"))
            .cmd("PFADD")
            .arg(key("unique_pubkeys"))
            .arg(pubkey)
            .cmd("HINCRBY")
            .arg(key("events_by_kind"))
            .arg(kind.to_string())
            .arg(1i64)
            .cmd("INCR")
            .arg(key("ingestion_window"))
            .query_async(&mut conn)
            .await;

        // Set TTL on the sliding window counter (resets every 60s for rate calc)
        let _: Result<i64, _> = conn.expire(&key("ingestion_window"), 60).await;
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
}
