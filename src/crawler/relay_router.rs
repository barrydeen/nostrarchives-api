use std::collections::HashMap;

use sqlx::PgPool;

use crate::error::AppError;

/// A relay preference from a kind-10002 event (NIP-65).
#[derive(Debug, Clone)]
pub struct RelayPreference {
    pub url: String,
    pub read: bool,
    pub write: bool,
}

/// Maps pubkeys to their preferred relay URLs based on NIP-65 relay lists.
pub struct RelayRouter {
    pool: PgPool,
}

/// Row returned when querying a single author's relay list.
#[derive(sqlx::FromRow)]
struct RelayListRow {
    tags: sqlx::types::Json<Vec<Vec<String>>>,
}

/// Row returned when querying batch author relay lists.
#[derive(sqlx::FromRow)]
struct BatchRelayListRow {
    pubkey: String,
    tags: sqlx::types::Json<Vec<Vec<String>>>,
}

impl RelayRouter {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Get relay preferences for a single author from their latest kind-10002 event.
    pub async fn get_author_relays(
        &self,
        pubkey: &str,
    ) -> Result<Vec<RelayPreference>, AppError> {
        let row = sqlx::query_as::<_, RelayListRow>(
            "SELECT tags FROM events WHERE pubkey = $1 AND kind = 10002 ORDER BY created_at DESC LIMIT 1",
        )
        .bind(pubkey)
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some(r) => Ok(parse_relay_tags(&r.tags)),
            None => Ok(vec![]),
        }
    }

    /// Batch-fetch relay preferences for multiple authors.
    pub async fn get_batch_author_relays(
        &self,
        pubkeys: &[String],
    ) -> Result<HashMap<String, Vec<RelayPreference>>, AppError> {
        if pubkeys.is_empty() {
            return Ok(HashMap::new());
        }

        let rows = sqlx::query_as::<_, BatchRelayListRow>(
            "SELECT DISTINCT ON (pubkey) pubkey, tags FROM events WHERE pubkey = ANY($1) AND kind = 10002 ORDER BY pubkey, created_at DESC",
        )
        .bind(pubkeys)
        .fetch_all(&self.pool)
        .await?;

        let mut map = HashMap::new();
        for row in rows {
            let prefs = parse_relay_tags(&row.tags);
            map.insert(row.pubkey, prefs);
        }

        Ok(map)
    }

    /// Group pubkeys by their preferred read relays.
    /// Returns `HashMap<relay_url, Vec<pubkey>>`.
    /// Only includes relays marked as read or read+write (not write-only).
    pub async fn get_relay_author_groups(
        &self,
        pubkeys: &[String],
    ) -> Result<HashMap<String, Vec<String>>, AppError> {
        let author_relays = self.get_batch_author_relays(pubkeys).await?;

        let mut groups: HashMap<String, Vec<String>> = HashMap::new();

        for (pubkey, prefs) in author_relays {
            for pref in prefs {
                if pref.read {
                    groups
                        .entry(pref.url)
                        .or_default()
                        .push(pubkey.clone());
                }
            }
        }

        Ok(groups)
    }

    /// Find the most popular relays across all kind-10002 events.
    /// Returns `(relay_url, user_count)` ordered by popularity.
    pub async fn get_top_relays(
        &self,
        limit: i64,
    ) -> Result<Vec<(String, i64)>, AppError> {
        // Extract relay URLs from JSONB tags across all kind-10002 events.
        // Each tag is an array like ["r", "wss://relay.example.com"] or ["r", "wss://...", "read"].
        // We use DISTINCT ON to only count the latest event per pubkey.
        let rows = sqlx::query_as::<_, (String, i64)>(
            r#"
            WITH latest_relay_lists AS (
                SELECT DISTINCT ON (pubkey) pubkey, tags
                FROM events
                WHERE kind = 10002
                ORDER BY pubkey, created_at DESC
            ),
            relay_urls AS (
                SELECT
                    lrl.pubkey,
                    tag ->> 1 AS relay_url
                FROM latest_relay_lists lrl,
                     jsonb_array_elements(lrl.tags::jsonb) AS tag
                WHERE tag ->> 0 = 'r'
                  AND tag ->> 1 IS NOT NULL
                  AND tag ->> 1 != ''
            )
            SELECT relay_url, COUNT(DISTINCT pubkey) AS user_count
            FROM relay_urls
            GROUP BY relay_url
            ORDER BY user_count DESC
            LIMIT $1
            "#,
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }
}

/// Parse NIP-65 relay tags into RelayPreference structs.
///
/// Tag formats:
/// - `["r", "wss://relay.example.com"]` → read + write
/// - `["r", "wss://relay.example.com", "read"]` → read only
/// - `["r", "wss://relay.example.com", "write"]` → write only
fn parse_relay_tags(tags: &[Vec<String>]) -> Vec<RelayPreference> {
    let mut prefs = Vec::new();

    for tag in tags {
        if tag.len() < 2 || tag[0] != "r" {
            continue;
        }

        let url = &tag[1];
        if url.is_empty() {
            continue;
        }

        let (read, write) = match tag.get(2).map(|s| s.as_str()) {
            Some("read") => (true, false),
            Some("write") => (false, true),
            _ => (true, true), // No marker means both read and write
        };

        prefs.push(RelayPreference {
            url: url.clone(),
            read,
            write,
        });
    }

    prefs
}
