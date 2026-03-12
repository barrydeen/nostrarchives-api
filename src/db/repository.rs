use std::collections::HashSet;

use sqlx::{PgPool, Row};

use super::models::{
    DailyStats, EventInteractions, EventQuery, EventRef, EventThread, KindCount, NewUser,
    NostrEvent, StoredEvent, TrendingNote, TrendingUser,
};
use crate::error::AppError;

#[derive(Clone)]
pub struct EventRepository {
    pool: PgPool,
}

#[derive(Debug, Clone)]
pub struct RankedEvent {
    pub event: StoredEvent,
    pub count: i64,
    pub total_sats: Option<i64>,
}

#[derive(Debug, sqlx::FromRow, Clone)]
pub struct ProfileRow {
    pub pubkey: String,
    pub content: String,
}

impl EventRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Return a clone of the underlying connection pool.
    pub fn pool(&self) -> PgPool {
        self.pool.clone()
    }

    /// Insert a new event. Returns true if the event was inserted (not a duplicate).
    pub async fn insert_event(
        &self,
        event: &NostrEvent,
        relay_url: &str,
    ) -> Result<bool, AppError> {
        let raw = serde_json::to_value(event).unwrap_or_default();
        let tags_json = serde_json::to_value(&event.tags).unwrap_or_default();

        let result = sqlx::query(
            "INSERT INTO events (id, pubkey, created_at, kind, content, sig, tags, raw, relay_url)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(&event.id)
        .bind(&event.pubkey)
        .bind(event.created_at)
        .bind(event.kind as i32)
        .bind(&event.content)
        .bind(&event.sig)
        .bind(&tags_json)
        .bind(&raw)
        .bind(relay_url)
        .execute(&self.pool)
        .await?;

        let inserted = result.rows_affected() > 0;

        if inserted {
            self.insert_tags(&event.id, &event.tags).await?;
            self.insert_refs(event).await?;
            // For zap receipts (kind 9735), extract the amount from the embedded
            // zap request in the "description" tag (NIP-57).
            if event.kind == 9735 {
                self.extract_zap_amount(event).await?;
            }
        }

        Ok(inserted)
    }

    /// Upsert the social graph edges for a follow list (kind 3) event.
    pub async fn upsert_follow_list(&self, event: &NostrEvent) -> Result<Option<usize>, AppError> {
        if event.kind != 3 {
            return Ok(None);
        }

        let mut seen = HashSet::new();
        let mut followees: Vec<(String, Option<String>)> = Vec::new();
        for tag in &event.tags {
            if tag.first().map(|v| v == "p").unwrap_or(false) {
                if let Some(target) = tag.get(1).filter(|v| !v.is_empty()) {
                    if !is_hex_pubkey(target) {
                        continue;
                    }
                    if seen.insert(target.clone()) {
                        let relay_hint = tag.get(2).filter(|s| !s.is_empty()).cloned();
                        followees.push((target.clone(), relay_hint));
                    }
                }
            }
        }

        let existing: Option<(i64,)> =
            sqlx::query_as("SELECT created_at FROM follow_lists WHERE pubkey = $1")
                .bind(&event.pubkey)
                .fetch_optional(&self.pool)
                .await?;

        if let Some((created_at,)) = existing {
            if created_at >= event.created_at {
                return Ok(None);
            }
        }

        let mut tx = self.pool.begin().await?;

        sqlx::query("DELETE FROM follows WHERE follower_pubkey = $1")
            .bind(&event.pubkey)
            .execute(&mut *tx)
            .await?;

        for (followed, relay_hint) in &followees {
            sqlx::query(
                "INSERT INTO follows (follower_pubkey, followed_pubkey, source_event_id, relay_hint, created_at)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (follower_pubkey, followed_pubkey) DO NOTHING",
            )
            .bind(&event.pubkey)
            .bind(followed)
            .bind(&event.id)
            .bind(relay_hint)
            .bind(event.created_at)
            .execute(&mut *tx)
            .await?;
        }

        sqlx::query(
            "INSERT INTO follow_lists (pubkey, event_id, created_at)
             VALUES ($1, $2, $3)
             ON CONFLICT (pubkey) DO UPDATE SET event_id = EXCLUDED.event_id, created_at = EXCLUDED.created_at, updated_at = NOW()",
        )
        .bind(&event.pubkey)
        .bind(&event.id)
        .bind(event.created_at)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        Ok(Some(followees.len()))
    }

    /// Extract and insert normalized tags for fast querying.
    async fn insert_tags(&self, event_id: &str, tags: &[Vec<String>]) -> Result<(), AppError> {
        for tag in tags {
            if tag.len() < 2 {
                continue;
            }
            let tag_name = &tag[0];
            let tag_value = &tag[1];
            let extra: Vec<&String> = tag.iter().skip(2).collect();
            let extra_json = serde_json::to_value(&extra).unwrap_or_default();

            sqlx::query(
                "INSERT INTO event_tags (event_id, tag_name, tag_value, extra_values)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT DO NOTHING",
            )
            .bind(event_id)
            .bind(tag_name)
            .bind(tag_value)
            .bind(&extra_json)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    /// Extract event references from tags and insert into event_refs.
    ///
    /// Reference types are determined by a combination of event kind and tag markers:
    /// - Kind 1 (note): `e` tags become reply/root/mention based on NIP-10 markers
    /// - Kind 7 (reaction): `e` tags become reaction refs
    /// - Kind 6 (repost): `e` tags become repost refs
    /// - Kind 9735 (zap receipt): `e` tags become zap refs
    /// - Other kinds: `e` tags become mention refs
    async fn insert_refs(&self, event: &NostrEvent) -> Result<(), AppError> {
        let e_tags: Vec<&Vec<String>> = event
            .tags
            .iter()
            .filter(|t| t.len() >= 2 && t[0] == "e")
            .collect();

        if e_tags.is_empty() {
            return Ok(());
        }

        for tag in &e_tags {
            let target_id = &tag[1];
            let relay_hint = tag.get(2).filter(|s| !s.is_empty()).cloned();
            let marker = tag.get(3).map(|s| s.as_str());

            let ref_type = match event.kind {
                7 => "reaction",
                6 | 16 => "repost",
                9735 => "zap",
                1 | 42 => {
                    // NIP-10 marker-based classification
                    match marker {
                        Some("root") => "root",
                        Some("reply") => "reply",
                        Some("mention") => "mention",
                        None => {
                            // Legacy positional: single e-tag = reply, first of many = root, last = reply
                            if e_tags.len() == 1 {
                                "reply"
                            } else if std::ptr::eq(*tag, *e_tags.first().unwrap()) {
                                "root"
                            } else if std::ptr::eq(*tag, *e_tags.last().unwrap()) {
                                "reply"
                            } else {
                                "mention"
                            }
                        }
                        _ => "mention",
                    }
                }
                _ => "mention",
            };

            sqlx::query(
                "INSERT INTO event_refs (source_event_id, target_event_id, ref_type, relay_hint, created_at)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT DO NOTHING",
            )
            .bind(&event.id)
            .bind(target_id)
            .bind(ref_type)
            .bind(&relay_hint)
            .bind(event.created_at)
            .execute(&self.pool)
            .await?;
        }

        Ok(())
    }

    /// Extract the zap amount from a kind-9735 zap receipt's embedded zap request.
    /// The "description" tag contains a JSON-encoded kind-9734 event whose tags
    /// include ["amount", "<msats>"]. We insert a synthetic amount tag into event_tags.
    async fn extract_zap_amount(&self, event: &NostrEvent) -> Result<(), AppError> {
        let description = event
            .tags
            .iter()
            .find(|t| t.len() >= 2 && t[0] == "description")
            .map(|t| &t[1]);

        let Some(desc_json) = description else {
            return Ok(());
        };

        let Ok(zap_request) = serde_json::from_str::<serde_json::Value>(desc_json) else {
            return Ok(());
        };

        let Some(tags) = zap_request.get("tags").and_then(|t| t.as_array()) else {
            return Ok(());
        };

        for tag in tags {
            let Some(arr) = tag.as_array() else { continue };
            if arr.len() >= 2 && arr[0].as_str() == Some("amount") && arr[1].as_str().is_some() {
                let amount = arr[1].as_str().unwrap();
                sqlx::query(
                    "INSERT INTO event_tags (event_id, tag_name, tag_value, extra_values)
                     VALUES ($1, 'amount', $2, '[]')
                     ON CONFLICT DO NOTHING",
                )
                .bind(&event.id)
                .bind(amount)
                .execute(&self.pool)
                .await?;
                break;
            }
        }

        Ok(())
    }

    /// Fetch the most recent kind-0 metadata event for each requested pubkey.
    pub async fn latest_profile_metadata(
        &self,
        pubkeys: &[String],
    ) -> Result<Vec<ProfileRow>, AppError> {
        if pubkeys.is_empty() {
            return Ok(vec![]);
        }

        let rows = sqlx::query_as::<_, ProfileRow>(
            "SELECT DISTINCT ON (pubkey) pubkey, content
             FROM events
             WHERE kind = 0 AND pubkey = ANY($1)
             ORDER BY pubkey, created_at DESC",
        )
        .bind(pubkeys)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    /// Given a list of pubkeys, return those that have NO kind-0 event stored.
    pub async fn pubkeys_missing_metadata(
        &self,
        pubkeys: &[String],
    ) -> Result<Vec<String>, AppError> {
        if pubkeys.is_empty() {
            return Ok(vec![]);
        }

        let existing: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT pubkey FROM events WHERE kind = 0 AND pubkey = ANY($1)",
        )
        .bind(pubkeys)
        .fetch_all(&self.pool)
        .await?;

        let existing_set: std::collections::HashSet<&str> =
            existing.iter().map(|s| s.as_str()).collect();

        Ok(pubkeys
            .iter()
            .filter(|pk| !existing_set.contains(pk.as_str()))
            .cloned()
            .collect())
    }

    /// Get a single event by ID.
    pub async fn get_event_by_id(&self, id: &str) -> Result<Option<StoredEvent>, AppError> {
        let event = sqlx::query_as::<_, StoredEvent>(
            "SELECT id, pubkey, created_at, kind, content, sig, tags, raw, relay_url, received_at
             FROM events WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(event)
    }

    /// Query events with optional filters.
    pub async fn query_events(&self, q: &EventQuery) -> Result<Vec<StoredEvent>, AppError> {
        let limit = q.limit.unwrap_or(50).min(500);
        let offset = q.offset.unwrap_or(0);

        let mut sql = String::from(
            "SELECT id, pubkey, created_at, kind, content, sig, tags, raw, relay_url, received_at
             FROM events WHERE 1=1",
        );
        let mut param_idx = 1u32;

        let mut conditions = Vec::new();
        let mut bind_pubkey = None;
        let mut bind_kind = None;
        let mut bind_since = None;
        let mut bind_until = None;
        let mut bind_search = None;

        if let Some(ref pubkey) = q.pubkey {
            conditions.push(format!("pubkey = ${param_idx}"));
            bind_pubkey = Some(pubkey.clone());
            param_idx += 1;
        }
        if let Some(kind) = q.kind {
            conditions.push(format!("kind = ${param_idx}"));
            bind_kind = Some(kind);
            param_idx += 1;
        }
        if let Some(since) = q.since {
            conditions.push(format!("created_at >= ${param_idx}"));
            bind_since = Some(since);
            param_idx += 1;
        }
        if let Some(until) = q.until {
            conditions.push(format!("created_at <= ${param_idx}"));
            bind_until = Some(until);
            param_idx += 1;
        }
        if let Some(ref search) = q.search {
            conditions.push(format!(
                "content_tsv @@ plainto_tsquery('english', ${param_idx})"
            ));
            bind_search = Some(search.clone());
            param_idx += 1;
        }

        for cond in &conditions {
            sql.push_str(&format!(" AND {cond}"));
        }

        sql.push_str(&format!(
            " ORDER BY created_at DESC LIMIT ${param_idx} OFFSET ${}",
            param_idx + 1
        ));

        // Build the query with dynamic binds
        let mut query = sqlx::query_as::<_, StoredEvent>(&sql);

        if let Some(ref v) = bind_pubkey {
            query = query.bind(v);
        }
        if let Some(v) = bind_kind {
            query = query.bind(v);
        }
        if let Some(v) = bind_since {
            query = query.bind(v);
        }
        if let Some(v) = bind_until {
            query = query.bind(v);
        }
        if let Some(ref v) = bind_search {
            query = query.bind(v);
        }

        query = query.bind(limit).bind(offset);

        let events = query.fetch_all(&self.pool).await?;
        Ok(events)
    }

    /// Count total events.
    pub async fn count_events(&self) -> Result<i64, AppError> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM events")
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    /// Count unique pubkeys.
    pub async fn count_unique_pubkeys(&self) -> Result<i64, AppError> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(DISTINCT pubkey) FROM events")
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    /// Get event counts by kind (top 20).
    pub async fn events_by_kind(&self) -> Result<Vec<KindCount>, AppError> {
        let rows = sqlx::query_as::<_, KindCount>(
            "SELECT kind, COUNT(*) as count FROM events GROUP BY kind ORDER BY count DESC LIMIT 20",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Get interaction counts for an event.
    pub async fn get_interactions(&self, event_id: &str) -> Result<EventInteractions, AppError> {
        let row = sqlx::query_as::<_, (i64, i64, i64, i64, i64)>(
            r#"
            SELECT
                COUNT(*) FILTER (WHERE r.ref_type = 'reply') AS replies,
                COUNT(*) FILTER (WHERE r.ref_type = 'reaction') AS reactions,
                COUNT(*) FILTER (WHERE r.ref_type = 'repost') AS reposts,
                COUNT(*) FILTER (WHERE r.ref_type = 'zap') AS zaps,
                COALESCE(
                    SUM(
                        CASE
                            WHEN r.ref_type = 'zap' THEN COALESCE(za.amount_msats, 0)
                            ELSE 0
                        END
                    ),
                    0
                )::bigint AS zap_total_msats
            FROM event_refs r
            LEFT JOIN LATERAL (
                SELECT
                    COALESCE(
                        MAX(
                            CASE
                                WHEN tag_value ~ '^[0-9]+$' THEN tag_value::bigint
                                ELSE 0
                            END
                        ),
                        0
                    ) AS amount_msats
                FROM event_tags
                WHERE event_id = r.source_event_id AND tag_name = 'amount'
            ) za ON TRUE
            WHERE r.target_event_id = $1
            "#,
        )
        .bind(event_id)
        .fetch_one(&self.pool)
        .await?;

        Ok(EventInteractions {
            replies: row.0,
            reactions: row.1,
            reposts: row.2,
            zaps: row.3,
            zap_sats: row.4 / 1000,
        })
    }

    /// Get events that reference a target event, filtered by ref_type.
    pub async fn get_referencing_events(
        &self,
        target_event_id: &str,
        ref_type: &str,
        limit: i64,
    ) -> Result<Vec<StoredEvent>, AppError> {
        let events = sqlx::query_as::<_, StoredEvent>(
            "SELECT e.id, e.pubkey, e.created_at, e.kind, e.content, e.sig, e.tags, e.raw, e.relay_url, e.received_at
             FROM events e
             INNER JOIN event_refs r ON r.source_event_id = e.id
             WHERE r.target_event_id = $1 AND r.ref_type = $2
             ORDER BY e.created_at DESC
             LIMIT $3",
        )
        .bind(target_event_id)
        .bind(ref_type)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(events)
    }

    /// Get the full thread context for an event: parent chain, interactions, and related events.
    pub async fn get_thread(
        &self,
        event_id: &str,
        limit: i64,
    ) -> Result<Option<EventThread>, AppError> {
        let event = match self.get_event_by_id(event_id).await? {
            Some(e) => e,
            None => return Ok(None),
        };

        // Find root and parent from this event's outgoing refs
        let refs = sqlx::query_as::<_, EventRef>(
            "SELECT source_event_id, target_event_id, ref_type, relay_hint, created_at
             FROM event_refs
             WHERE source_event_id = $1 AND ref_type IN ('root', 'reply')",
        )
        .bind(event_id)
        .fetch_all(&self.pool)
        .await?;

        let root_id = refs
            .iter()
            .find(|r| r.ref_type == "root")
            .map(|r| r.target_event_id.clone());
        let parent_id = refs
            .iter()
            .find(|r| r.ref_type == "reply")
            .map(|r| r.target_event_id.clone());

        let interactions = self.get_interactions(event_id).await?;
        let replies = self
            .get_referencing_events(event_id, "reply", limit)
            .await?;
        let reactions = self
            .get_referencing_events(event_id, "reaction", limit)
            .await?;
        let reposts = self
            .get_referencing_events(event_id, "repost", limit)
            .await?;
        let zaps = self.get_referencing_events(event_id, "zap", limit).await?;

        Ok(Some(EventThread {
            event,
            root_id,
            parent_id,
            interactions,
            replies,
            reactions,
            reposts,
            zaps,
        }))
    }

    /// List pubkeys that the given pubkey follows.
    pub async fn list_follows(
        &self,
        pubkey: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<String>, AppError> {
        let rows = sqlx::query_scalar::<_, String>(
            "SELECT followed_pubkey
             FROM follows
             WHERE follower_pubkey = $1
             ORDER BY created_at DESC
             LIMIT $2 OFFSET $3",
        )
        .bind(pubkey)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    /// List pubkeys that follow the given pubkey.
    pub async fn list_followers(
        &self,
        pubkey: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<String>, AppError> {
        let rows = sqlx::query_scalar::<_, String>(
            "SELECT follower_pubkey
             FROM follows
             WHERE followed_pubkey = $1
             ORDER BY created_at DESC
             LIMIT $2 OFFSET $3",
        )
        .bind(pubkey)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    /// Return (follows_count, followers_count) for a pubkey.
    pub async fn follow_counts(&self, pubkey: &str) -> Result<(i64, i64), AppError> {
        let follows_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM follows WHERE follower_pubkey = $1")
                .bind(pubkey)
                .fetch_one(&self.pool)
                .await?;

        let followers_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM follows WHERE followed_pubkey = $1")
                .bind(pubkey)
                .fetch_one(&self.pool)
                .await?;

        Ok((follows_count, followers_count))
    }

    /// Return ranked note events by ref_type (reaction/zap) with optional since filter.
    pub async fn top_notes_by_ref(
        &self,
        ref_type: &str,
        since: Option<i64>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<RankedEvent>, AppError> {
        let is_zap = ref_type == "zap";

        let rows = sqlx::query(
            r#"
            WITH zap_amounts AS (
                SELECT
                    event_id,
                    MAX(
                        CASE
                            WHEN tag_value ~ '^[0-9]+$' THEN tag_value::bigint
                            ELSE 0
                        END
                    ) AS amount_msats
                FROM event_tags
                WHERE tag_name = 'amount'
                GROUP BY event_id
            )
            SELECT
                e.id,
                e.pubkey,
                e.created_at,
                e.kind,
                e.content,
                e.sig,
                e.tags,
                e.raw,
                e.relay_url,
                e.received_at,
                counts.metric_count,
                counts.zap_total_msats
            FROM (
                SELECT
                    r.target_event_id,
                    COUNT(*) AS metric_count,
                    SUM(COALESCE(za.amount_msats, 0))::bigint AS zap_total_msats
                FROM event_refs r
                LEFT JOIN zap_amounts za ON za.event_id = r.source_event_id
                WHERE r.ref_type = $1
                  AND ($2::bigint IS NULL OR r.created_at >= $2)
                GROUP BY r.target_event_id
                ORDER BY
                    CASE WHEN $1 = 'zap' THEN SUM(COALESCE(za.amount_msats, 0))::bigint
                         ELSE COUNT(*)
                    END DESC
                LIMIT $3 OFFSET $4
            ) counts
            JOIN events e ON e.id = counts.target_event_id
            WHERE e.kind = 1
            ORDER BY
                CASE WHEN $1 = 'zap' THEN counts.zap_total_msats
                     ELSE counts.metric_count
                END DESC,
                e.created_at DESC
            "#,
        )
        .bind(ref_type)
        .bind(since)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        let events = rows
            .into_iter()
            .map(|row| -> Result<RankedEvent, sqlx::Error> {
                let event = StoredEvent {
                    id: row.try_get("id")?,
                    pubkey: row.try_get("pubkey")?,
                    created_at: row.try_get("created_at")?,
                    kind: row.try_get("kind")?,
                    content: row.try_get("content")?,
                    sig: row.try_get("sig")?,
                    tags: row.try_get("tags")?,
                    raw: row.try_get("raw")?,
                    relay_url: row.try_get("relay_url").ok(),
                    received_at: row.try_get("received_at")?,
                };

                let count = row.try_get("metric_count")?;
                let zap_total_msats: i64 = row.try_get("zap_total_msats")?;
                let total_sats = if is_zap {
                    Some(zap_total_msats / 1000)
                } else {
                    None
                };

                Ok(RankedEvent {
                    event,
                    count,
                    total_sats,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(events)
    }

    /// Trending notes: composite score combining zaps (1 point per sat), reposts (1000),
    /// replies (500), and reactions (100). 24h window.
    pub async fn trending_notes(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<TrendingNote>, AppError> {
        let since = chrono::Utc::now().timestamp() - 86400;

        let rows = sqlx::query(
            r#"
            WITH zap_amounts AS (
                SELECT
                    event_id,
                    MAX(
                        CASE WHEN tag_value ~ '^[0-9]+$' THEN tag_value::bigint ELSE 0 END
                    ) AS amount_msats
                FROM event_tags
                WHERE tag_name = 'amount'
                GROUP BY event_id
            ),
            note_engagement AS (
                SELECT
                    r.target_event_id,
                    COUNT(*) FILTER (WHERE r.ref_type = 'zap')      AS zap_count,
                    COALESCE(SUM(CASE WHEN r.ref_type = 'zap' THEN COALESCE(za.amount_msats, 0) / 1000 ELSE 0 END), 0)::bigint AS zap_sats,
                    COUNT(*) FILTER (WHERE r.ref_type = 'repost')   AS repost_count,
                    COUNT(*) FILTER (WHERE r.ref_type = 'reply')    AS reply_count,
                    COUNT(*) FILTER (WHERE r.ref_type = 'reaction') AS reaction_count
                FROM event_refs r
                LEFT JOIN zap_amounts za ON za.event_id = r.source_event_id
                WHERE r.created_at >= $1
                GROUP BY r.target_event_id
            )
            SELECT
                e.id, e.pubkey, e.created_at, e.kind, e.content, e.sig, e.tags, e.raw,
                e.relay_url, e.received_at,
                ne.zap_sats,
                ne.repost_count,
                ne.reply_count,
                ne.reaction_count,
                (ne.zap_sats + ne.repost_count * 1000 + ne.reply_count * 500 + ne.reaction_count * 100)::bigint AS score
            FROM note_engagement ne
            JOIN events e ON e.id = ne.target_event_id AND e.kind = 1
            ORDER BY score DESC, e.created_at DESC
            LIMIT $2 OFFSET $3
            "#,
        )
        .bind(since)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        let notes = rows
            .into_iter()
            .map(|row| -> Result<TrendingNote, sqlx::Error> {
                let event = StoredEvent {
                    id: row.try_get("id")?,
                    pubkey: row.try_get("pubkey")?,
                    created_at: row.try_get("created_at")?,
                    kind: row.try_get("kind")?,
                    content: row.try_get("content")?,
                    sig: row.try_get("sig")?,
                    tags: row.try_get("tags")?,
                    raw: row.try_get("raw")?,
                    relay_url: row.try_get("relay_url").ok(),
                    received_at: row.try_get("received_at")?,
                };
                Ok(TrendingNote {
                    event,
                    score: row.try_get("score")?,
                    zap_sats: row.try_get("zap_sats")?,
                    reposts: row.try_get("repost_count")?,
                    replies: row.try_get("reply_count")?,
                    reactions: row.try_get("reaction_count")?,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(notes)
    }

    /// New users: pubkeys whose earliest event is within the last 24h.
    pub async fn new_users(&self, limit: i64, offset: i64) -> Result<Vec<NewUser>, AppError> {
        let since = chrono::Utc::now().timestamp() - 86400;

        let rows = sqlx::query_as::<_, (String, i64, i64)>(
            r#"
            SELECT pubkey, MIN(created_at) AS first_seen, COUNT(*) AS event_count
            FROM events
            GROUP BY pubkey
            HAVING MIN(created_at) >= $1
            ORDER BY first_seen DESC
            LIMIT $2 OFFSET $3
            "#,
        )
        .bind(since)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(pubkey, first_seen, event_count)| NewUser {
                pubkey,
                first_seen,
                event_count,
            })
            .collect())
    }

    /// Trending users: pubkeys that gained the most new followers in the last 24h.
    /// Uses follows.created_at (from the kind-3 contact list event timestamp).
    pub async fn trending_users(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<TrendingUser>, AppError> {
        let since = chrono::Utc::now().timestamp() - 86400;

        let rows = sqlx::query_as::<_, (String, i64)>(
            r#"
            SELECT followed_pubkey, COUNT(DISTINCT follower_pubkey) AS new_followers
            FROM follows
            WHERE created_at >= $1
            GROUP BY followed_pubkey
            ORDER BY new_followers DESC
            LIMIT $2 OFFSET $3
            "#,
        )
        .bind(since)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(pubkey, new_followers)| TrendingUser {
                pubkey,
                new_followers,
            })
            .collect())
    }

    /// Daily stats: DAU, total sats sent, daily posts (last 24h).
    pub async fn daily_stats(&self) -> Result<DailyStats, AppError> {
        let since = chrono::Utc::now().timestamp() - 86400;

        let row = sqlx::query_as::<_, (i64, i64)>(
            r#"
            SELECT
                COUNT(DISTINCT pubkey) AS daily_active_users,
                COUNT(*) FILTER (WHERE kind = 1) AS daily_posts
            FROM events
            WHERE created_at >= $1
            "#,
        )
        .bind(since)
        .fetch_one(&self.pool)
        .await?;

        // Total sats: sum of zap receipt amounts in last 24h
        let total_sats: i64 = sqlx::query_scalar(
            r#"
            SELECT COALESCE(SUM(
                CASE WHEN et.tag_value ~ '^[0-9]+$' THEN et.tag_value::bigint ELSE 0 END
            ) / 1000, 0)::bigint
            FROM events e
            JOIN event_tags et ON et.event_id = e.id AND et.tag_name = 'amount'
            WHERE e.kind = 9735 AND e.created_at >= $1
            "#,
        )
        .bind(since)
        .fetch_one(&self.pool)
        .await?;

        Ok(DailyStats {
            daily_active_users: row.0,
            total_sats_sent: total_sats,
            daily_posts: row.1,
        })
    }

    /// Search profiles with ranked results.
    ///
    /// Ranking algorithm:
    /// - Exact name match: +100,000
    /// - Prefix match: +10,000
    /// - NIP-05 match: +5,000
    /// - Trigram similarity: 0-100
    /// - Follower influence: ln(followers + 1) * 50
    /// - Engagement influence: ln(engagement + 1) * 20
    /// - Recency bonus: +200 if active in last 7d, +100 if last 30d
    pub async fn search_profiles(
        &self,
        query: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<super::models::ProfileSearchResult>, AppError> {
        let rows = sqlx::query(
            r#"
            SELECT
                pubkey, name, display_name, nip05, about, picture,
                follower_count, engagement_score, last_active_at,
                (
                    CASE WHEN LOWER(name) = LOWER($1) THEN 100000 ELSE 0 END +
                    CASE WHEN LOWER(display_name) = LOWER($1) THEN 100000 ELSE 0 END +
                    CASE WHEN LOWER(nip05) = LOWER($1) THEN 80000 ELSE 0 END +
                    CASE WHEN name ILIKE $1 || '%' THEN 10000 ELSE 0 END +
                    CASE WHEN display_name ILIKE $1 || '%' THEN 10000 ELSE 0 END +
                    CASE WHEN nip05 ILIKE $1 || '%' THEN 5000 ELSE 0 END +
                    GREATEST(
                        COALESCE(similarity(name, $1), 0),
                        COALESCE(similarity(display_name, $1), 0)
                    ) * 100 +
                    LN(GREATEST(follower_count, 0) + 1) * 50 +
                    LN(GREATEST(engagement_score, 0) + 1) * 20 +
                    CASE
                        WHEN last_active_at > EXTRACT(EPOCH FROM NOW())::bigint - 604800 THEN 200
                        WHEN last_active_at > EXTRACT(EPOCH FROM NOW())::bigint - 2592000 THEN 100
                        ELSE 0
                    END
                )::float8 AS rank_score
            FROM profile_search
            WHERE
                name ILIKE '%' || $1 || '%'
                OR display_name ILIKE '%' || $1 || '%'
                OR nip05 ILIKE '%' || $1 || '%'
            ORDER BY rank_score DESC, follower_count DESC
            LIMIT $2 OFFSET $3
            "#,
        )
        .bind(query)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        let results = rows
            .into_iter()
            .map(
                |row| -> Result<super::models::ProfileSearchResult, sqlx::Error> {
                    Ok(super::models::ProfileSearchResult {
                        pubkey: row.try_get("pubkey")?,
                        name: row.try_get("name")?,
                        display_name: row.try_get("display_name")?,
                        nip05: row.try_get("nip05")?,
                        about: row.try_get("about")?,
                        picture: row.try_get("picture")?,
                        follower_count: row.try_get("follower_count")?,
                        engagement_score: row.try_get("engagement_score")?,
                        last_active_at: row.try_get("last_active_at")?,
                        rank_score: row.try_get("rank_score")?,
                    })
                },
            )
            .collect::<Result<Vec<_>, _>>()?;

        Ok(results)
    }

    /// Lightweight profile suggestion for autocomplete.
    /// Prioritizes prefix matches, weighted by follower count + engagement.
    pub async fn suggest_profiles(
        &self,
        query: &str,
        limit: i64,
    ) -> Result<Vec<super::models::ProfileSearchResult>, AppError> {
        let rows = sqlx::query(
            r#"
            SELECT
                pubkey, name, display_name, nip05, NULL::text AS about, picture,
                follower_count, engagement_score, last_active_at,
                (
                    CASE
                        WHEN LOWER(name) = LOWER($1) OR LOWER(display_name) = LOWER($1) THEN 100000
                        WHEN name ILIKE $1 || '%' OR display_name ILIKE $1 || '%' THEN 10000
                        WHEN nip05 ILIKE $1 || '%' THEN 5000
                        ELSE 100
                    END
                    + LN(GREATEST(follower_count, 0) + 1) * 50
                    + LN(GREATEST(engagement_score, 0) + 1) * 20
                )::float8 AS rank_score
            FROM profile_search
            WHERE
                name ILIKE '%' || $1 || '%'
                OR display_name ILIKE '%' || $1 || '%'
                OR nip05 ILIKE '%' || $1 || '%'
                OR pubkey LIKE $1 || '%'
            ORDER BY rank_score DESC, follower_count DESC
            LIMIT $2
            "#,
        )
        .bind(query)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        let results = rows
            .into_iter()
            .map(
                |row| -> Result<super::models::ProfileSearchResult, sqlx::Error> {
                    Ok(super::models::ProfileSearchResult {
                        pubkey: row.try_get("pubkey")?,
                        name: row.try_get("name")?,
                        display_name: row.try_get("display_name")?,
                        nip05: row.try_get("nip05")?,
                        about: row.try_get("about")?,
                        picture: row.try_get("picture")?,
                        follower_count: row.try_get("follower_count")?,
                        engagement_score: row.try_get("engagement_score")?,
                        last_active_at: row.try_get("last_active_at")?,
                        rank_score: row.try_get("rank_score")?,
                    })
                },
            )
            .collect::<Result<Vec<_>, _>>()?;

        Ok(results)
    }

    /// Search notes with full-text search, ranked by relevance and engagement.
    ///
    /// Ranking algorithm:
    /// - FTS relevance (ts_rank): ×1000
    /// - Engagement score: ln(weighted_engagement + 1) × 10
    /// - Recency bonus: +50 for <24h, +25 for <7d
    pub async fn search_notes(
        &self,
        query: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<super::models::NoteSearchResult>, AppError> {
        let rows = sqlx::query(
            r#"
            WITH ranked AS (
                SELECT
                    e.id, e.pubkey, e.created_at, e.kind, e.content, e.sig,
                    e.tags, e.raw, e.relay_url, e.received_at,
                    ts_rank(e.content_tsv, query) AS text_rank
                FROM events e, plainto_tsquery('english', $1) query
                WHERE e.kind = 1 AND e.content_tsv @@ query
                ORDER BY ts_rank(e.content_tsv, query) DESC
                LIMIT 200
            )
            SELECT
                r.id, r.pubkey, r.created_at, r.kind, r.content, r.sig,
                r.tags, r.raw, r.relay_url, r.received_at,
                COALESCE(eng.reaction_count, 0)::bigint AS reactions,
                COALESCE(eng.reply_count, 0)::bigint AS replies,
                COALESCE(eng.repost_count, 0)::bigint AS reposts,
                COALESCE(eng.zap_count, 0)::bigint AS zaps,
                (
                    r.text_rank * 1000 +
                    LN(
                        COALESCE(eng.reaction_count, 0) * 100 +
                        COALESCE(eng.reply_count, 0) * 500 +
                        COALESCE(eng.repost_count, 0) * 1000 +
                        COALESCE(eng.zap_count, 0) * 2000 + 1
                    ) * 10 +
                    CASE
                        WHEN r.created_at > EXTRACT(EPOCH FROM NOW())::bigint - 86400 THEN 50
                        WHEN r.created_at > EXTRACT(EPOCH FROM NOW())::bigint - 604800 THEN 25
                        ELSE 0
                    END
                )::float8 AS rank_score
            FROM ranked r
            LEFT JOIN LATERAL (
                SELECT
                    COUNT(*) FILTER (WHERE ref_type = 'reaction') AS reaction_count,
                    COUNT(*) FILTER (WHERE ref_type = 'reply') AS reply_count,
                    COUNT(*) FILTER (WHERE ref_type = 'repost') AS repost_count,
                    COUNT(*) FILTER (WHERE ref_type = 'zap') AS zap_count
                FROM event_refs
                WHERE target_event_id = r.id
            ) eng ON TRUE
            ORDER BY rank_score DESC
            LIMIT $2 OFFSET $3
            "#,
        )
        .bind(query)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        let results = rows
            .into_iter()
            .map(
                |row| -> Result<super::models::NoteSearchResult, sqlx::Error> {
                    let event = StoredEvent {
                        id: row.try_get("id")?,
                        pubkey: row.try_get("pubkey")?,
                        created_at: row.try_get("created_at")?,
                        kind: row.try_get("kind")?,
                        content: row.try_get("content")?,
                        sig: row.try_get("sig")?,
                        tags: row.try_get("tags")?,
                        raw: row.try_get("raw")?,
                        relay_url: row.try_get("relay_url").ok(),
                        received_at: row.try_get("received_at")?,
                    };
                    Ok(super::models::NoteSearchResult {
                        event,
                        rank_score: row.try_get("rank_score")?,
                        reactions: row.try_get("reactions")?,
                        replies: row.try_get("replies")?,
                        reposts: row.try_get("reposts")?,
                        zaps: row.try_get("zaps")?,
                    })
                },
            )
            .collect::<Result<Vec<_>, _>>()?;

        Ok(results)
    }

    /// Resolve a 64-char hex string: check if it's a known event id or pubkey.
    /// Returns ("event", id) or ("profile", pubkey) or None.
    pub async fn resolve_hex(&self, hex: &str) -> Result<Option<(&'static str, String)>, AppError> {
        // Check event first (more specific)
        let event_exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM events WHERE id = $1)")
                .bind(hex)
                .fetch_one(&self.pool)
                .await?;
        if event_exists {
            return Ok(Some(("event", hex.to_string())));
        }

        // Check pubkey
        let pubkey_exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM events WHERE pubkey = $1 LIMIT 1)")
                .bind(hex)
                .fetch_one(&self.pool)
                .await?;
        if pubkey_exists {
            return Ok(Some(("profile", hex.to_string())));
        }

        Ok(None)
    }

    /// Refresh the profile_search materialized view (CONCURRENTLY to avoid blocking reads).
    pub async fn refresh_profile_search(&self) -> Result<(), AppError> {
        sqlx::query("REFRESH MATERIALIZED VIEW CONCURRENTLY profile_search")
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

fn is_hex_pubkey(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|c| c.is_ascii_hexdigit())
}
