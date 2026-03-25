use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::RwLock;
use sqlx::PgPool;

use crate::error::AppError;

/// Blocked entry returned by list methods.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BlockedEntry {
    pub value: String,
    pub reason: Option<String>,
    pub blocked_at: chrono::DateTime<chrono::Utc>,
    pub blocked_by: String,
}

/// In-memory cache for moderation block lists, loaded from DB on startup.
/// Mutations update both the in-memory set and the database.
#[derive(Clone)]
pub struct BlockCache {
    blocked_pubkeys: Arc<RwLock<HashSet<String>>>,
    blocked_hashtags: Arc<RwLock<HashSet<String>>>,
    blocked_search_terms: Arc<RwLock<HashSet<String>>>,
    pool: PgPool,
}

impl BlockCache {
    pub fn new(pool: PgPool) -> Self {
        Self {
            blocked_pubkeys: Arc::new(RwLock::new(HashSet::new())),
            blocked_hashtags: Arc::new(RwLock::new(HashSet::new())),
            blocked_search_terms: Arc::new(RwLock::new(HashSet::new())),
            pool,
        }
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

    /// Delete all data for a blocked pubkey from the database.
    /// Returns count of deleted events.
    pub async fn delete_pubkey_data(&self, pubkey: &str) -> Result<i64, AppError> {
        let mut tx = self.pool.begin().await?;

        // Get event IDs for this pubkey first (needed for cascading deletes)
        let event_ids: Vec<(String,)> = sqlx::query_as(
            "SELECT id FROM events WHERE pubkey = $1",
        )
        .bind(pubkey)
        .fetch_all(&mut *tx)
        .await?;

        let ids: Vec<String> = event_ids.into_iter().map(|(id,)| id).collect();
        let event_count = ids.len() as i64;

        if !ids.is_empty() {
            // Delete event_refs where this pubkey's events are source or target
            sqlx::query(
                "DELETE FROM event_refs WHERE source_event_id = ANY($1) OR target_event_id = ANY($1)",
            )
            .bind(&ids)
            .execute(&mut *tx)
            .await?;

            // Delete from note_hashtags
            sqlx::query("DELETE FROM note_hashtags WHERE event_id = ANY($1)")
                .bind(&ids)
                .execute(&mut *tx)
                .await?;

            // Delete from search_index
            sqlx::query("DELETE FROM search_index WHERE event_id = ANY($1)")
                .bind(&ids)
                .execute(&mut *tx)
                .await?;

            // Delete the events themselves
            sqlx::query("DELETE FROM events WHERE pubkey = $1")
                .bind(pubkey)
                .execute(&mut *tx)
                .await?;
        }

        // Delete social graph data
        sqlx::query("DELETE FROM follows WHERE follower_pubkey = $1 OR followed_pubkey = $1")
            .bind(pubkey)
            .execute(&mut *tx)
            .await?;

        sqlx::query("DELETE FROM follow_lists WHERE pubkey = $1")
            .bind(pubkey)
            .execute(&mut *tx)
            .await?;

        // Delete crawler state
        sqlx::query("DELETE FROM crawl_state WHERE pubkey = $1")
            .bind(pubkey)
            .execute(&mut *tx)
            .await?;

        // Delete relay lists
        sqlx::query("DELETE FROM relay_lists WHERE pubkey = $1")
            .bind(pubkey)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;

        tracing::info!(pubkey, events_deleted = event_count, "Pubkey data purged");
        Ok(event_count)
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
