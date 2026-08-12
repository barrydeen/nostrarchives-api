use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use sqlx::PgPool;

use crate::cache::StatsCache;
use crate::error::AppError;

/// Blocked entry returned by list methods.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BlockedEntry {
    pub value: String,
    pub reason: Option<String>,
    pub blocked_at: chrono::DateTime<chrono::Utc>,
    pub blocked_by: String,
}

/// Status of a background purge job.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PurgeStatus {
    pub pubkey: String,
    pub state: PurgeState,
    pub events_deleted: i64,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PurgeState {
    Queued,
    Running,
    Completed,
    Failed,
}

/// In-memory cache for moderation block lists, loaded from DB on startup.
/// Mutations update both the in-memory set and the database.
/// Data deletion runs in a background worker via a purge queue.
#[derive(Clone)]
pub struct BlockCache {
    blocked_pubkeys: Arc<RwLock<HashSet<String>>>,
    blocked_hashtags: Arc<RwLock<HashSet<String>>>,
    blocked_search_terms: Arc<RwLock<HashSet<String>>>,
    pool: PgPool,
    purge_tx: mpsc::UnboundedSender<String>,
    purge_statuses: Arc<RwLock<HashMap<String, PurgeStatus>>>,
}

impl BlockCache {
    pub fn new(pool: PgPool) -> Self {
        let (purge_tx, purge_rx) = mpsc::unbounded_channel();
        let purge_statuses = Arc::new(RwLock::new(HashMap::new()));

        let cache = Self {
            blocked_pubkeys: Arc::new(RwLock::new(HashSet::new())),
            blocked_hashtags: Arc::new(RwLock::new(HashSet::new())),
            blocked_search_terms: Arc::new(RwLock::new(HashSet::new())),
            pool,
            purge_tx,
            purge_statuses,
        };

        // Stash rx for later — worker is spawned after StatsCache is available
        // We leak the rx into a static so spawn_purge_worker can pick it up.
        // Actually, we'll store it differently — see spawn_purge_worker.
        PURGE_RX.lock().unwrap().replace(purge_rx);

        cache
    }

    /// Load all block lists from the database into memory.
    pub async fn initialize(&self) -> Result<(), AppError> {
        let pubkeys: Vec<(String,)> =
            sqlx::query_as("SELECT pubkey FROM blocked_pubkeys")
                .fetch_all(&self.pool)
                .await?;
        let hashtags: Vec<(String,)> =
            sqlx::query_as("SELECT hashtag FROM blocked_hashtags")
                .fetch_all(&self.pool)
                .await?;
        let terms: Vec<(String,)> =
            sqlx::query_as("SELECT term FROM blocked_search_terms")
                .fetch_all(&self.pool)
                .await?;

        {
            let mut set = self.blocked_pubkeys.write().await;
            *set = pubkeys.into_iter().map(|(p,)| p).collect();
        }
        {
            let mut set = self.blocked_hashtags.write().await;
            *set = hashtags.into_iter().map(|(h,)| h.to_lowercase()).collect();
        }
        {
            let mut set = self.blocked_search_terms.write().await;
            *set = terms.into_iter().map(|(t,)| t.to_lowercase()).collect();
        }

        let pk_count = self.blocked_pubkeys.read().await.len();
        let ht_count = self.blocked_hashtags.read().await.len();
        let st_count = self.blocked_search_terms.read().await.len();
        tracing::info!(
            blocked_pubkeys = pk_count,
            blocked_hashtags = ht_count,
            blocked_search_terms = st_count,
            "Block cache initialized"
        );

        Ok(())
    }

    /// Spawn the background purge worker. Must be called once after StatsCache is available.
    pub fn spawn_purge_worker(&self, cache: StatsCache) {
        let purge_rx = PURGE_RX
            .lock()
            .unwrap()
            .take()
            .expect("spawn_purge_worker called twice or before new()");

        let pool = self.pool.clone();
        let statuses = self.purge_statuses.clone();

        tokio::spawn(async move {
            purge_worker(purge_rx, pool, cache, statuses).await;
        });

        tracing::info!("purge worker started");
    }

    // ── Pubkey operations ──

    pub async fn is_pubkey_blocked(&self, pubkey: &str) -> bool {
        self.blocked_pubkeys.read().await.contains(pubkey)
    }

    pub async fn block_pubkey(
        &self,
        pubkey: &str,
        reason: Option<&str>,
        blocked_by: &str,
    ) -> Result<(), AppError> {
        sqlx::query(
            "INSERT INTO blocked_pubkeys (pubkey, reason, blocked_by)
             VALUES ($1, $2, $3)
             ON CONFLICT (pubkey) DO UPDATE SET reason = $2, blocked_at = NOW()",
        )
        .bind(pubkey)
        .bind(reason)
        .bind(blocked_by)
        .execute(&self.pool)
        .await?;

        self.blocked_pubkeys.write().await.insert(pubkey.to_string());
        tracing::info!(pubkey, "Pubkey blocked");
        Ok(())
    }

    /// Queue background deletion of all data for a blocked pubkey.
    pub async fn queue_purge(&self, pubkey: &str) {
        let status = PurgeStatus {
            pubkey: pubkey.to_string(),
            state: PurgeState::Queued,
            events_deleted: 0,
            started_at: chrono::Utc::now(),
            finished_at: None,
            error: None,
        };
        self.purge_statuses.write().await.insert(pubkey.to_string(), status);
        let _ = self.purge_tx.send(pubkey.to_string());
    }

    /// Get the purge status for a pubkey, if any.
    pub async fn purge_status(&self, pubkey: &str) -> Option<PurgeStatus> {
        self.purge_statuses.read().await.get(pubkey).cloned()
    }

    pub async fn unblock_pubkey(&self, pubkey: &str) -> Result<bool, AppError> {
        let result = sqlx::query("DELETE FROM blocked_pubkeys WHERE pubkey = $1")
            .bind(pubkey)
            .execute(&self.pool)
            .await?;

        self.blocked_pubkeys.write().await.remove(pubkey);
        Ok(result.rows_affected() > 0)
    }

    pub async fn list_blocked_pubkeys(&self) -> Result<Vec<BlockedEntry>, AppError> {
        let rows: Vec<(String, Option<String>, chrono::DateTime<chrono::Utc>, String)> =
            sqlx::query_as(
                "SELECT pubkey, reason, blocked_at, blocked_by FROM blocked_pubkeys ORDER BY blocked_at DESC",
            )
            .fetch_all(&self.pool)
            .await?;

        Ok(rows
            .into_iter()
            .map(|(value, reason, blocked_at, blocked_by)| BlockedEntry {
                value,
                reason,
                blocked_at,
                blocked_by,
            })
            .collect())
    }

    // ── Hashtag operations ──

    pub async fn is_hashtag_blocked(&self, tag: &str) -> bool {
        self.blocked_hashtags.read().await.contains(&tag.to_lowercase())
    }

    pub async fn blocked_hashtags_snapshot(&self) -> HashSet<String> {
        self.blocked_hashtags.read().await.clone()
    }

    pub async fn block_hashtag(
        &self,
        hashtag: &str,
        reason: Option<&str>,
        blocked_by: &str,
    ) -> Result<(), AppError> {
        let lower = hashtag.to_lowercase();
        sqlx::query(
            "INSERT INTO blocked_hashtags (hashtag, reason, blocked_by)
             VALUES ($1, $2, $3)
             ON CONFLICT (hashtag) DO UPDATE SET reason = $2, blocked_at = NOW()",
        )
        .bind(&lower)
        .bind(reason)
        .bind(blocked_by)
        .execute(&self.pool)
        .await?;

        self.blocked_hashtags.write().await.insert(lower.clone());
        tracing::info!(hashtag = lower.as_str(), "Hashtag blocked from trending");
        Ok(())
    }

    pub async fn unblock_hashtag(&self, hashtag: &str) -> Result<bool, AppError> {
        let lower = hashtag.to_lowercase();
        let result = sqlx::query("DELETE FROM blocked_hashtags WHERE hashtag = $1")
            .bind(&lower)
            .execute(&self.pool)
            .await?;

        self.blocked_hashtags.write().await.remove(&lower);
        Ok(result.rows_affected() > 0)
    }

    pub async fn list_blocked_hashtags(&self) -> Result<Vec<BlockedEntry>, AppError> {
        let rows: Vec<(String, Option<String>, chrono::DateTime<chrono::Utc>, String)> =
            sqlx::query_as(
                "SELECT hashtag, reason, blocked_at, blocked_by FROM blocked_hashtags ORDER BY blocked_at DESC",
            )
            .fetch_all(&self.pool)
            .await?;

        Ok(rows
            .into_iter()
            .map(|(value, reason, blocked_at, blocked_by)| BlockedEntry {
                value,
                reason,
                blocked_at,
                blocked_by,
            })
            .collect())
    }

    // ── Search term operations ──

    pub async fn is_search_term_blocked(&self, term: &str) -> bool {
        self.blocked_search_terms
            .read()
            .await
            .contains(&term.to_lowercase())
    }

    pub async fn block_search_term(
        &self,
        term: &str,
        reason: Option<&str>,
        blocked_by: &str,
    ) -> Result<(), AppError> {
        let lower = term.to_lowercase();
        sqlx::query(
            "INSERT INTO blocked_search_terms (term, reason, blocked_by)
             VALUES ($1, $2, $3)
             ON CONFLICT (term) DO UPDATE SET reason = $2, blocked_at = NOW()",
        )
        .bind(&lower)
        .bind(reason)
        .bind(blocked_by)
        .execute(&self.pool)
        .await?;

        self.blocked_search_terms.write().await.insert(lower.clone());
        tracing::info!(term = lower.as_str(), "Search term blocked");
        Ok(())
    }

    pub async fn unblock_search_term(&self, term: &str) -> Result<bool, AppError> {
        let lower = term.to_lowercase();
        let result = sqlx::query("DELETE FROM blocked_search_terms WHERE term = $1")
            .bind(&lower)
            .execute(&self.pool)
            .await?;

        self.blocked_search_terms.write().await.remove(&lower);
        Ok(result.rows_affected() > 0)
    }

    pub async fn list_blocked_search_terms(&self) -> Result<Vec<BlockedEntry>, AppError> {
        let rows: Vec<(String, Option<String>, chrono::DateTime<chrono::Utc>, String)> =
            sqlx::query_as(
                "SELECT term, reason, blocked_at, blocked_by FROM blocked_search_terms ORDER BY blocked_at DESC",
            )
            .fetch_all(&self.pool)
            .await?;

        Ok(rows
            .into_iter()
            .map(|(value, reason, blocked_at, blocked_by)| BlockedEntry {
                value,
                reason,
                blocked_at,
                blocked_by,
            })
            .collect())
    }
}

// Global slot to pass the receiver from new() to spawn_purge_worker().
// This is necessary because BlockCache is Clone (needed for AppState) but
// mpsc::UnboundedReceiver is not Clone.
static PURGE_RX: std::sync::Mutex<Option<mpsc::UnboundedReceiver<String>>> =
    std::sync::Mutex::new(None);

/// Background worker that processes purge jobs sequentially.
async fn purge_worker(
    mut rx: mpsc::UnboundedReceiver<String>,
    pool: PgPool,
    cache: StatsCache,
    statuses: Arc<RwLock<HashMap<String, PurgeStatus>>>,
) {
    const BATCH_SIZE: i64 = 5000;

    while let Some(pubkey) = rx.recv().await {
        // Mark as running
        {
            let mut map = statuses.write().await;
            if let Some(status) = map.get_mut(&pubkey) {
                status.state = PurgeState::Running;
            }
        }

        tracing::info!(pubkey = pubkey.as_str(), "Purge job started");

        let result = purge_pubkey_data(&pool, &pubkey, BATCH_SIZE, &statuses).await;

        match result {
            Ok(total) => {
                // Note the old per-pubkey `profiles:metadata:{pubkey}` call could
                // never match: that key is `profiles:metadata:{hash}` of the
                // requested pubkey *set* (handlers.rs), so it has to go by prefix.
                invalidate_purge_caches(&cache).await;

                let mut map = statuses.write().await;
                if let Some(status) = map.get_mut(&pubkey) {
                    status.state = PurgeState::Completed;
                    status.events_deleted = total;
                    status.finished_at = Some(chrono::Utc::now());
                }

                tracing::info!(pubkey = pubkey.as_str(), events_deleted = total, "Purge job completed");
            }
            Err(e) => {
                let mut map = statuses.write().await;
                if let Some(status) = map.get_mut(&pubkey) {
                    status.state = PurgeState::Failed;
                    status.error = Some(e.to_string());
                    status.finished_at = Some(chrono::Utc::now());
                }

                tracing::error!(pubkey = pubkey.as_str(), error = %e, "Purge job failed");
            }
        }
    }
}

/// Delete all data for a pubkey in batches. Updates status with progress.
async fn purge_pubkey_data(
    pool: &PgPool,
    pubkey: &str,
    batch_size: i64,
    statuses: &Arc<RwLock<HashMap<String, PurgeStatus>>>,
) -> Result<i64, AppError> {
    purge_pubkey_data_with(pool, pubkey, batch_size, &mut |total| {
        let statuses = statuses.clone();
        let pubkey = pubkey.to_string();
        tokio::spawn(async move {
            let mut map = statuses.write().await;
            if let Some(status) = map.get_mut(&pubkey) {
                status.events_deleted = total;
            }
        });
    })
    .await
}

/// Delete all data for a pubkey in batches, reporting progress through a
/// callback.
///
/// This is the single deletion path: the admin block endpoint and the bulk ban
/// tool both go through it, so the two can't drift apart and leave one of them
/// missing a table.
///
/// The caller MUST have inserted the pubkey into `blocked_pubkeys` first.
/// Deleting without blocking is self-reversing: negentropy builds its local ID
/// set from `events`, so deleted ids come back as `need_ids` on the next sync,
/// and the `missing_events` drainer re-fetches them directly.
pub async fn purge_pubkey_data_with(
    pool: &PgPool,
    pubkey: &str,
    batch_size: i64,
    progress: &mut (dyn FnMut(i64) + Send),
) -> Result<i64, AppError> {
    let mut total_deleted: i64 = 0;

    loop {
        let batch: Vec<(String,)> = sqlx::query_as(
            "SELECT id FROM events WHERE pubkey = $1 LIMIT $2",
        )
        .bind(pubkey)
        .bind(batch_size)
        .fetch_all(pool)
        .await?;

        if batch.is_empty() {
            break;
        }

        let ids: Vec<String> = batch.into_iter().map(|(id,)| id).collect();
        let batch_count = ids.len() as i64;

        let mut tx = pool.begin().await?;

        // Stage the reply_count decrements before event_refs is deleted — the
        // refs are the only record of which parent each reply pointed at.
        // DISTINCT ON with this ORDER BY reproduces insert_refs' target choice
        // (prefer the `reply` ref, else the first `root` ref), and the
        // is_machine_note join reproduces the exclusion at repository.rs:631.
        sqlx::query(
            "CREATE TEMP TABLE IF NOT EXISTS reply_decrements (
                 target_event_id TEXT PRIMARY KEY,
                 n INTEGER NOT NULL
             )",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "WITH chosen AS (
                 SELECT DISTINCT ON (r.source_event_id)
                        r.source_event_id, r.target_event_id
                 FROM event_refs r
                 JOIN events s ON s.id = r.source_event_id
                 WHERE r.source_event_id = ANY($1)
                   AND r.ref_type IN ('reply', 'root')
                   AND NOT s.is_machine_note
                 ORDER BY r.source_event_id, (r.ref_type = 'reply') DESC
             )
             INSERT INTO reply_decrements (target_event_id, n)
             SELECT target_event_id, COUNT(*)
             FROM chosen
             WHERE target_event_id <> ALL($1)
             GROUP BY target_event_id
             ON CONFLICT (target_event_id)
                 DO UPDATE SET n = reply_decrements.n + EXCLUDED.n",
        )
        .bind(&ids)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "DELETE FROM event_refs WHERE source_event_id = ANY($1) OR target_event_id = ANY($1)",
        )
        .bind(&ids)
        .execute(&mut *tx)
        .await?;

        sqlx::query("DELETE FROM note_hashtags WHERE event_id = ANY($1)")
            .bind(&ids)
            .execute(&mut *tx)
            .await?;

        sqlx::query("DELETE FROM search_index WHERE event_id = ANY($1)")
            .bind(&ids)
            .execute(&mut *tx)
            .await?;

        // Without this the missing-events drainer actively re-downloads every
        // note we just deleted (src/main.rs queue drain).
        sqlx::query("DELETE FROM missing_events WHERE event_id = ANY($1)")
            .bind(&ids)
            .execute(&mut *tx)
            .await?;

        sqlx::query("DELETE FROM seen_events WHERE event_id = ANY($1) OR target_id = ANY($1)")
            .bind(&ids)
            .execute(&mut *tx)
            .await?;

        sqlx::query(
            "DELETE FROM zap_metadata WHERE event_id = ANY($1) OR zapped_event_id = ANY($1)",
        )
        .bind(&ids)
        .execute(&mut *tx)
        .await?;

        sqlx::query("DELETE FROM events WHERE id = ANY($1)")
            .bind(&ids)
            .execute(&mut *tx)
            .await?;

        // Apply the staged decrements to parents that survived this batch.
        // GREATEST guards against pre-existing counter drift.
        sqlx::query(
            "UPDATE events e
             SET reply_count = GREATEST(0, e.reply_count - d.n)
             FROM reply_decrements d
             WHERE e.id = d.target_event_id",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query("DELETE FROM reply_decrements")
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;

        total_deleted += batch_count;
        progress(total_deleted);

        tracing::info!(pubkey, batch = batch_count, total = total_deleted, "Purge batch completed");
    }

    // Delete social graph and crawler data
    sqlx::query("DELETE FROM follows WHERE follower_pubkey = $1 OR followed_pubkey = $1")
        .bind(pubkey)
        .execute(pool)
        .await?;

    sqlx::query("DELETE FROM follow_lists WHERE pubkey = $1")
        .bind(pubkey)
        .execute(pool)
        .await?;

    sqlx::query("DELETE FROM crawl_state WHERE pubkey = $1")
        .bind(pubkey)
        .execute(pool)
        .await?;

    // Zaps the pubkey sent or received: kind 9735 receipts are authored by the
    // zapper service, not the bot, so deleting events by author never reaches
    // these rows.
    sqlx::query("DELETE FROM zap_metadata WHERE sender_pubkey = $1 OR recipient_pubkey = $1")
        .bind(pubkey)
        .execute(pool)
        .await?;

    sqlx::query("DELETE FROM scheduled_events WHERE pubkey = $1")
        .bind(pubkey)
        .execute(pool)
        .await?;

    sqlx::query("DELETE FROM wot_scores WHERE pubkey = $1")
        .bind(pubkey)
        .execute(pool)
        .await?;

    Ok(total_deleted)
}

/// Redis key prefixes holding data that can contain a purged author's content.
///
/// The per-pubkey keys are not enough: trending, hashtag, analytics, client and
/// ws feeds are all keyed by query parameters rather than by author.
pub const PURGE_CACHE_PREFIXES: &[&str] = &[
    "profile:notes",
    "profile:replies",
    "profile:zaps_sent",
    "profile:zaps_recv",
    "profile:zap_stats",
    "profiles:metadata",
    "home",
    "trending",
    "hashtag",
    "analytics",
    "clients",
    "relays",
    "ws",
    "search:suggest",
    "feeds",
];

/// Flush every cache prefix that can hold purged content.
///
/// Uses `delete_by_prefix` (SCAN-based) rather than `invalidate_pattern`, which
/// uses KEYS and blocks the whole Redis instance.
pub async fn invalidate_purge_caches(cache: &StatsCache) {
    for prefix in PURGE_CACHE_PREFIXES {
        cache.delete_by_prefix(prefix).await;
    }
}
