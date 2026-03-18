use sqlx::PgPool;

use crate::error::AppError;

/// A crawl target pulled from the work queue.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CrawlTarget {
    pub pubkey: String,
    pub follower_count: i64,
    pub priority_tier: i16,
    pub crawl_cursor: Option<i64>,
    pub newest_seen_at: Option<i64>,
    pub notes_crawled: i64,
}

/// Manages the crawl_state table: seeding, prioritization, and work queue.
#[derive(Clone)]
pub struct CrawlQueue {
    pool: PgPool,
}

impl CrawlQueue {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Seed the crawl queue from WoT scores.
    /// Seed the crawl queue from follows table, then upgrade tiers using WoT scores.
    ///
    /// Always seeds from raw follows first (ensures all followed authors are queued),
    /// then upgrades priority tiers for authors that pass WoT. This avoids the
    /// chicken-and-egg problem where WoT scores are sparse on a fresh database.
    pub async fn sync_from_follows(&self) -> Result<SyncStats, AppError> {
        // Step 1: Always seed from raw follows (the complete follower graph)
        let base_stats = self.sync_from_raw_follows().await?;

        // Step 2: Upgrade tiers using WoT scores if available
        let wot_upgraded = sqlx::query(
            r#"
            UPDATE crawl_state cs
            SET
                priority_tier = CASE
                    WHEN ws.quality_followers >= 1000 THEN 1
                    WHEN ws.quality_followers >= 100  THEN 2
                    WHEN ws.quality_followers >= 21   THEN 3
                    ELSE cs.priority_tier
                END,
                updated_at = NOW()
            FROM wot_scores ws
            WHERE cs.pubkey = ws.pubkey
              AND ws.passes_wot = true
              AND cs.priority_tier > CASE
                    WHEN ws.quality_followers >= 1000 THEN 1
                    WHEN ws.quality_followers >= 100  THEN 2
                    WHEN ws.quality_followers >= 21   THEN 3
                    ELSE 99
                END
            "#,
        )
        .execute(&self.pool)
        .await?
        .rows_affected();

        if wot_upgraded > 0 {
            tracing::info!(upgraded = wot_upgraded, "crawl queue: upgraded tiers from WoT scores");
        }

        Ok(base_stats)
    }

    /// Seed from raw follows table.
    async fn sync_from_raw_follows(&self) -> Result<SyncStats, AppError> {
        let inserted = sqlx::query(
            r#"
            WITH follower_counts AS (
                SELECT followed_pubkey AS pubkey, COUNT(*) AS cnt
                FROM follows
                GROUP BY followed_pubkey
            )
            INSERT INTO crawl_state (pubkey, follower_count, priority_tier)
            SELECT
                fc.pubkey,
                fc.cnt,
                CASE
                    WHEN fc.cnt >= 1000 THEN 1
                    WHEN fc.cnt >= 100  THEN 2
                    WHEN fc.cnt >= 10   THEN 3
                    ELSE 4
                END
            FROM follower_counts fc
            WHERE NOT EXISTS (
                SELECT 1 FROM crawl_state cs WHERE cs.pubkey = fc.pubkey
            )
            "#,
        )
        .execute(&self.pool)
        .await?
        .rows_affected() as i64;

        let updated = sqlx::query(
            r#"
            UPDATE crawl_state cs
            SET
                follower_count = fc.cnt,
                priority_tier = CASE
                    WHEN fc.cnt >= 1000 THEN 1
                    WHEN fc.cnt >= 100  THEN 2
                    WHEN fc.cnt >= 10   THEN 3
                    ELSE 4
                END,
                updated_at = NOW()
            FROM (
                SELECT followed_pubkey AS pubkey, COUNT(*) AS cnt
                FROM follows
                GROUP BY followed_pubkey
            ) fc
            WHERE cs.pubkey = fc.pubkey
              AND (cs.follower_count != fc.cnt)
            "#,
        )
        .execute(&self.pool)
        .await?
        .rows_affected();

        Ok(SyncStats {
            new_pubkeys: inserted,
            updated_counts: updated as i64,
        })
    }

    /// Populate newest_seen_at from existing events so we don't re-crawl what we have.
    pub async fn sync_cursors_from_events(&self) -> Result<u64, AppError> {
        let result = sqlx::query(
            r#"
            UPDATE crawl_state cs
            SET
                newest_seen_at = ev.max_ts,
                updated_at = NOW()
            FROM (
                SELECT pubkey, MAX(created_at) AS max_ts
                FROM events
                WHERE kind = 1
                GROUP BY pubkey
            ) ev
            WHERE cs.pubkey = ev.pubkey
              AND (cs.newest_seen_at IS NULL OR cs.newest_seen_at < ev.max_ts)
            "#,
        )
        .execute(&self.pool)
        .await?
        .rows_affected();

        Ok(result)
    }

    /// Pull the next batch of authors to crawl, ordered by priority tier then next_crawl_at.
    /// Atomically marks them with a future next_crawl_at to prevent double-picking.
    pub async fn take_batch(&self, batch_size: i64) -> Result<Vec<CrawlTarget>, AppError> {
        let targets = sqlx::query_as::<_, CrawlTarget>(
            r#"
            WITH picked AS (
                SELECT pubkey
                FROM crawl_state
                WHERE next_crawl_at <= NOW()
                ORDER BY priority_tier ASC, follower_count DESC, next_crawl_at ASC
                LIMIT $1
                FOR UPDATE SKIP LOCKED
            )
            UPDATE crawl_state cs
            SET next_crawl_at = NOW() + INTERVAL '10 minutes'
            FROM picked
            WHERE cs.pubkey = picked.pubkey
            RETURNING cs.pubkey, cs.follower_count, cs.priority_tier,
                      cs.crawl_cursor, cs.newest_seen_at, cs.notes_crawled
            "#,
        )
        .bind(batch_size)
        .fetch_all(&self.pool)
        .await?;

        Ok(targets)
    }

    /// Update crawl state after successfully crawling an author.
    pub async fn mark_crawled(
        &self,
        pubkey: &str,
        notes_found: i64,
        oldest_fetched: Option<i64>,
        newest_fetched: Option<i64>,
    ) -> Result<(), AppError> {
        // Calculate next crawl interval based on priority tier and results.
        // High-follower authors get recrawled sooner for recent content.
        // Authors with no new results get pushed further back.
        sqlx::query(
            r#"
            UPDATE crawl_state
            SET
                notes_crawled = notes_crawled + $2,
                crawl_cursor = CASE
                    WHEN $3::bigint IS NOT NULL AND (crawl_cursor IS NULL OR $3 < crawl_cursor)
                    THEN $3
                    ELSE crawl_cursor
                END,
                newest_seen_at = CASE
                    WHEN $4::bigint IS NOT NULL AND (newest_seen_at IS NULL OR $4 > newest_seen_at)
                    THEN $4
                    ELSE newest_seen_at
                END,
                last_crawled_at = NOW(),
                next_crawl_at = NOW() + CASE
                    WHEN $2 = 0 AND priority_tier = 1 THEN INTERVAL '1 hour'
                    WHEN $2 = 0 AND priority_tier = 2 THEN INTERVAL '4 hours'
                    WHEN $2 = 0 AND priority_tier = 3 THEN INTERVAL '12 hours'
                    WHEN $2 = 0 THEN INTERVAL '24 hours'
                    WHEN priority_tier = 1 THEN INTERVAL '15 minutes'
                    WHEN priority_tier = 2 THEN INTERVAL '30 minutes'
                    WHEN priority_tier = 3 THEN INTERVAL '2 hours'
                    ELSE INTERVAL '6 hours'
                END,
                updated_at = NOW()
            WHERE pubkey = $1
            "#,
        )
        .bind(pubkey)
        .bind(notes_found)
        .bind(oldest_fetched)
        .bind(newest_fetched)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Get queue stats for monitoring.
    pub async fn stats(&self) -> Result<QueueStats, AppError> {
        let row = sqlx::query_as::<_, QueueStatsRow>(
            r#"
            SELECT
                COUNT(*) AS total,
                COUNT(*) FILTER (WHERE priority_tier = 1) AS tier1,
                COUNT(*) FILTER (WHERE priority_tier = 2) AS tier2,
                COUNT(*) FILTER (WHERE priority_tier = 3) AS tier3,
                COUNT(*) FILTER (WHERE priority_tier = 4) AS tier4,
                COUNT(*) FILTER (WHERE last_crawled_at IS NOT NULL) AS crawled,
                COUNT(*) FILTER (WHERE next_crawl_at <= NOW()) AS ready,
                COALESCE(SUM(notes_crawled), 0)::bigint AS total_notes
            FROM crawl_state
            "#,
        )
        .fetch_one(&self.pool)
        .await?;

        Ok(QueueStats {
            total_authors: row.total,
            tier1_authors: row.tier1,
            tier2_authors: row.tier2,
            tier3_authors: row.tier3,
            tier4_authors: row.tier4,
            authors_crawled: row.crawled,
            ready_to_crawl: row.ready,
            total_notes_crawled: row.total_notes,
        })
    }
}

#[derive(Debug)]
pub struct SyncStats {
    pub new_pubkeys: i64,
    pub updated_counts: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct QueueStatsRow {
    total: i64,
    tier1: i64,
    tier2: i64,
    tier3: i64,
    tier4: i64,
    crawled: i64,
    ready: i64,
    total_notes: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct QueueStats {
    pub total_authors: i64,
    pub tier1_authors: i64,
    pub tier2_authors: i64,
    pub tier3_authors: i64,
    pub tier4_authors: i64,
    pub authors_crawled: i64,
    pub ready_to_crawl: i64,
    pub total_notes_crawled: i64,
}
