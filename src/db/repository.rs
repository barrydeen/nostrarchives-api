use std::collections::HashSet;

use sqlx::{PgPool, Row};

use super::models::{
    DailyStats, EventInteractions, EventQuery, EventRef, EventThread, KindCount,
    NewUser, NostrEvent, StoredEvent,
    TrendingNote, TrendingUser,
};
use crate::block_cache::BlockCache;
use crate::error::AppError;
use crate::follower_cache::FollowerCache;
use crate::wot_cache::WotCache;

#[derive(Clone)]
pub struct EventRepository {
    pool: PgPool,
    pub follower_cache: FollowerCache,
    pub wot_cache: WotCache,
    pub block_cache: BlockCache,
}

#[derive(Debug, Clone)]
pub struct RankedEvent {
    pub event: StoredEvent,
    pub count: i64,
    pub total_sats: Option<i64>,
    pub reactions: i64,
    pub replies: i64,
    pub reposts: i64,
    pub zap_sats: i64,
}

/// A missing event ID discovered during ingestion (e.g. zap target we don't have).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MissingEvent {
    pub event_id: String,
    pub relay_hint: Option<String>,
    pub priority: i16,
}

#[derive(Debug, sqlx::FromRow, Clone)]
pub struct ProfileRow {
    pub pubkey: String,
    pub content: String,
}

/// Convert a range string to a timestamp for filtering.

/// Calculate cache TTL in seconds based on range.
fn range_cache_ttl(range: &str) -> u64 {
    match range {
        "today" => 300,    // 5 min
        "7d" => 1800,      // 30 min
        "30d" => 3600,     // 1 hour
        "all" => 86400,    // 1 day
        _ => 1800,         // default to 30 min
    }
}

impl EventRepository {
    pub fn new(pool: PgPool, follower_cache: FollowerCache, wot_cache: WotCache, block_cache: BlockCache) -> Self {
        Self { pool, follower_cache, wot_cache, block_cache }
    }

    /// Return a clone of the underlying connection pool.
    pub fn pool(&self) -> PgPool {
        self.pool.clone()
    }

    /// Insert a new event with kind-based routing.
    ///
    /// v2 branching:
    /// - Kind 0 (metadata): Store if author passes WoT OR we already have their events
    /// - Kind 1 (note): WoT check. Store event, insert refs, increment reply_count on target
    /// - Kind 3 (contact list): ALWAYS process for social graph (upsert-only, one per pubkey)
    /// - Kind 6/16 (repost): Counter-only. Increment repost_count, do NOT store event
    /// - Kind 7 (reaction): Counter-only. Increment reaction_count, do NOT store event
    /// - Kind 9735 (zap): ALWAYS store regardless of WoT. Increment zap counters on target
    /// - Kind 10002 (relay list): ALWAYS process. Upsert-only, one per pubkey
    pub async fn insert_event(
        &self,
        event: &NostrEvent,
        relay_url: &str,
    ) -> Result<bool, AppError> {
        // Reject events from blocked pubkeys
        if self.block_cache.is_pubkey_blocked(&event.pubkey).await {
            return Ok(false);
        }

        match event.kind {
            // Kind 6/7/16: counter-only, never stored as full events
            6 | 16 => {
                return self.process_repost_as_counter(event).await;
            }
            7 => {
                return self.process_reaction_as_counter(event).await;
            }
            // Kind 0 (metadata): store if WoT passes, or follower cache passes, or we already have their events
            0 => {
                if !self.passes_quality_check(&event.pubkey).await? {
                    let has_events: bool = sqlx::query_scalar(
                        "SELECT EXISTS(SELECT 1 FROM events WHERE pubkey = $1 LIMIT 1)",
                    )
                    .bind(&event.pubkey)
                    .fetch_one(&self.pool)
                    .await?;
                    if !has_events {
                        return Ok(false);
                    }
                }
            }
            // Kind 3 (contact list): always process for social graph
            3 => { /* always allow */ }
            // Kind 9735 (zap): always store, bypass WoT
            9735 => { /* always allow */ }
            // Kind 10002 (relay list): always process
            10002 => { /* always allow */ }
            // Kind 1 (note) and others: require quality check
            _ => {
                if !self.passes_quality_check(&event.pubkey).await? {
                    tracing::debug!(
                        pubkey = %event.pubkey,
                        kind = event.kind,
                        "Skipping event from author not passing quality check"
                    );
                    return Ok(false);
                }
            }
        }

        // For kind-3: upsert-only (keep only latest per pubkey)
        if event.kind == 3 {
            return self.upsert_kind3_event(event, relay_url).await;
        }

        // For kind-10002: upsert-only (keep only latest per pubkey)
        if event.kind == 10002 {
            return self.upsert_relay_list_event(event, relay_url).await;
        }

        // Standard storage for kinds 0, 1, 9735, etc.
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
            // Only insert refs for kind-1 (notes)
            if event.kind == 1 {
                self.insert_refs(event).await?;
            }
            // For zap receipts, extract metadata and increment counters
            if event.kind == 9735 {
                self.extract_zap_metadata(event).await?;
                self.increment_zap_counters(event).await?;
            }
        }

        Ok(inserted)
    }

    /// Check if a pubkey passes quality filtering via two-level WoT check.
    async fn passes_quality_check(&self, pubkey: &str) -> Result<bool, AppError> {
        self.wot_cache.passes_wot(pubkey).await
    }

    /// Process a repost (kind 6/16) as a counter increment only.
    /// Does NOT store the repost event. Uses seen_events for dedup.
    async fn process_repost_as_counter(&self, event: &NostrEvent) -> Result<bool, AppError> {
        let e_tag = event
            .tags
            .iter()
            .find(|t| t.len() >= 2 && t[0] == "e");

        let Some(e_tag) = e_tag else {
            return Ok(false);
        };
        let target_id = e_tag[1].clone();
        let relay_hint = e_tag.get(2).filter(|s| !s.is_empty()).cloned();

        // Dedup via seen_events
        if self.check_seen(&event.id).await? {
            return Ok(false);
        }

        // Increment counter on target event
        let updated = sqlx::query(
            "UPDATE events SET repost_count = repost_count + 1 WHERE id = $1",
        )
        .bind(&target_id)
        .execute(&self.pool)
        .await?;

        if updated.rows_affected() > 0 {
            self.mark_seen(&event.id, event.kind as i16, &target_id, event.created_at)
                .await?;
            return Ok(true);
        }

        // Target not in DB — queue it for on-demand fetch
        let _ = self.queue_missing_event(&target_id, relay_hint.as_deref(), 1).await;
        Ok(false)
    }

    /// Process a reaction (kind 7) as a counter increment only.
    /// Does NOT store the reaction event. Uses seen_events for dedup.
    async fn process_reaction_as_counter(&self, event: &NostrEvent) -> Result<bool, AppError> {
        let e_tag = event
            .tags
            .iter()
            .rev()
            .find(|t| t.len() >= 2 && t[0] == "e");

        let Some(e_tag) = e_tag else {
            return Ok(false);
        };
        let target_id = e_tag[1].clone();
        let relay_hint = e_tag.get(2).filter(|s| !s.is_empty()).cloned();

        // Dedup via seen_events
        if self.check_seen(&event.id).await? {
            return Ok(false);
        }

        // Increment counter on target event
        let updated = sqlx::query(
            "UPDATE events SET reaction_count = reaction_count + 1 WHERE id = $1",
        )
        .bind(&target_id)
        .execute(&self.pool)
        .await?;

        if updated.rows_affected() > 0 {
            self.mark_seen(&event.id, event.kind as i16, &target_id, event.created_at)
                .await?;
            return Ok(true);
        }

        // Target not in DB — queue it for on-demand fetch
        let _ = self.queue_missing_event(&target_id, relay_hint.as_deref(), 1).await;
        Ok(false)
    }

    /// Increment zap_count and zap_amount_msats on the target event for a zap receipt.
    async fn increment_zap_counters(&self, event: &NostrEvent) -> Result<(), AppError> {
        let target_id = event
            .tags
            .iter()
            .find(|t| t.len() >= 2 && t[0] == "e")
            .and_then(|t| t.get(1))
            .cloned();

        let Some(target_id) = target_id else {
            return Ok(());
        };

        // Extract amount from the description tag (zap request)
        let mut amount_msats: i64 = 0;
        if let Some(desc_tag) = event.tags.iter().find(|t| t.len() >= 2 && t[0] == "description")
        {
            if let Ok(zap_request) = serde_json::from_str::<serde_json::Value>(&desc_tag[1]) {
                if let Some(tags) = zap_request.get("tags").and_then(|t| t.as_array()) {
                    for tag in tags {
                        let Some(arr) = tag.as_array() else { continue };
                        if arr.len() >= 2 && arr[0].as_str() == Some("amount") {
                            if let Some(raw) = arr[1].as_str() {
                                if let Ok(parsed) = raw.parse::<i64>() {
                                    amount_msats = parsed;
                                }
                                break;
                            }
                        }
                    }
                }
            }
        }

        // If amount is still 0, try to parse from bolt11 tag
        if amount_msats == 0 {
            if let Some(bolt11_tag) = event.tags.iter().find(|t| t.len() >= 2 && t[0] == "bolt11") {
                if let Some(bolt11) = bolt11_tag.get(1) {
                    if let Some(parsed_amount) = self.parse_bolt11_amount(bolt11) {
                        amount_msats = parsed_amount;
                    }
                }
            }
        }

        let updated = sqlx::query(
            "UPDATE events SET zap_count = zap_count + 1, zap_amount_msats = zap_amount_msats + $2 WHERE id = $1",
        )
        .bind(&target_id)
        .bind(amount_msats)
        .execute(&self.pool)
        .await?;

        if updated.rows_affected() == 0 {
            // Zap target not in DB — high priority fetch (broken zap linkage)
            let relay_hint = event
                .tags
                .iter()
                .find(|t| t.len() >= 2 && t[0] == "e")
                .and_then(|t| t.get(2))
                .filter(|s| !s.is_empty())
                .map(|s| s.as_str());
            let _ = self.queue_missing_event(&target_id, relay_hint, 2).await;
        }

        Ok(())
    }

    /// Check if an event ID has already been processed (seen_events dedup).
    async fn check_seen(&self, event_id: &str) -> Result<bool, AppError> {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM seen_events WHERE event_id = $1)")
                .bind(event_id)
                .fetch_one(&self.pool)
                .await?;
        Ok(exists)
    }

    /// Record an event as seen in the dedup table.
    async fn mark_seen(
        &self,
        event_id: &str,
        kind: i16,
        target_id: &str,
        created_at: i64,
    ) -> Result<(), AppError> {
        sqlx::query(
            "INSERT INTO seen_events (event_id, kind, target_id, created_at)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (event_id) DO NOTHING",
        )
        .bind(event_id)
        .bind(kind)
        .bind(target_id)
        .bind(created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Upsert a kind-3 event: keep only the latest per pubkey in the events table.
    async fn upsert_kind3_event(
        &self,
        event: &NostrEvent,
        relay_url: &str,
    ) -> Result<bool, AppError> {
        // Check if we have a newer kind-3 from this pubkey
        let existing: Option<(i64,)> = sqlx::query_as(
            "SELECT created_at FROM events WHERE pubkey = $1 AND kind = 3 ORDER BY created_at DESC LIMIT 1",
        )
        .bind(&event.pubkey)
        .fetch_optional(&self.pool)
        .await?;

        if let Some((existing_ts,)) = existing {
            if existing_ts >= event.created_at {
                return Ok(false); // We have a newer one
            }
            // Delete the old one
            sqlx::query("DELETE FROM events WHERE pubkey = $1 AND kind = 3")
                .bind(&event.pubkey)
                .execute(&self.pool)
                .await?;
        }

        // Insert the new one
        let raw = serde_json::to_value(event).unwrap_or_default();
        let tags_json = serde_json::to_value(&event.tags).unwrap_or_default();

        sqlx::query(
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

        Ok(true)
    }

    /// Upsert a kind-10002 relay list event: keep only the latest per pubkey.
    async fn upsert_relay_list_event(
        &self,
        event: &NostrEvent,
        relay_url: &str,
    ) -> Result<bool, AppError> {
        let existing: Option<(i64,)> = sqlx::query_as(
            "SELECT created_at FROM events WHERE pubkey = $1 AND kind = 10002 ORDER BY created_at DESC LIMIT 1",
        )
        .bind(&event.pubkey)
        .fetch_optional(&self.pool)
        .await?;

        if let Some((existing_ts,)) = existing {
            if existing_ts >= event.created_at {
                return Ok(false);
            }
            sqlx::query("DELETE FROM events WHERE pubkey = $1 AND kind = 10002")
                .bind(&event.pubkey)
                .execute(&self.pool)
                .await?;
        }

        let raw = serde_json::to_value(event).unwrap_or_default();
        let tags_json = serde_json::to_value(&event.tags).unwrap_or_default();

        sqlx::query(
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

        Ok(true)
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

    /// Extract event references from tags and insert into event_refs.
    /// v2: Only processes kind-1 note refs (reply/root/mention).
    /// Reactions, reposts, and zaps are handled as counter increments instead.
    async fn insert_refs(&self, event: &NostrEvent) -> Result<(), AppError> {
        // v2: only insert refs for kind-1 notes
        if event.kind != 1 {
            return Ok(());
        }

        let e_tags: Vec<&Vec<String>> = event
            .tags
            .iter()
            .filter(|t| t.len() >= 2 && t[0] == "e")
            .collect();

        if e_tags.is_empty() {
            return Ok(());
        }

        let mut has_reply_ref = false;
        let mut reply_target_id: Option<String> = None;

        for tag in &e_tags {
            let target_id = &tag[1];
            let relay_hint = tag.get(2).filter(|s| !s.is_empty()).cloned();
            let marker = tag.get(3).map(|s| s.as_str());

            let ref_type = match marker {
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
            };

            if ref_type == "reply" || ref_type == "root" {
                has_reply_ref = true;
                // Track the reply target for counter increment
                if ref_type == "reply" || (ref_type == "root" && reply_target_id.is_none()) {
                    reply_target_id = Some(target_id.clone());
                }
            }

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

        if has_reply_ref {
            sqlx::query("UPDATE events SET is_reply = true WHERE id = $1")
                .bind(&event.id)
                .execute(&self.pool)
                .await?;

            // v2: increment reply_count on the target event
            if let Some(ref target_id) = reply_target_id {
                sqlx::query("UPDATE events SET reply_count = reply_count + 1 WHERE id = $1")
                    .bind(target_id)
                    .execute(&self.pool)
                    .await?;
            }
        }

        Ok(())
    }

    /// Parse amount from bolt11 invoice string.
    /// Format: lnbc<amount><multiplier>1<separator>...
    /// Multipliers: m (milli=10^-3), u (micro=10^-6), n (nano=10^-9), p (pico=10^-12)
    /// Amount is in BTC, convert to msats (1 BTC = 100_000_000_000 msats)
    fn parse_bolt11_amount(&self, bolt11: &str) -> Option<i64> {
        use regex::Regex;
        
        let re = Regex::new(r"lnbc(\d+)([munp])1").ok()?;
        let captures = re.captures(bolt11)?;
        
        let amount_str = captures.get(1)?.as_str();
        let amount: u64 = amount_str.parse().ok()?;
        
        let multiplier = captures.get(2)?.as_str();
        let btc_amount = match multiplier {
            "m" => amount as f64 * 1e-3,  // milli
            "u" => amount as f64 * 1e-6,  // micro
            "n" => amount as f64 * 1e-9,  // nano
            "p" => amount as f64 * 1e-12, // pico
            _ => return None,
        };
        
        // Convert BTC to msats (1 BTC = 100_000_000_000 msats)
        let msats = (btc_amount * 100_000_000_000.0) as i64;
        Some(msats)
    }

    /// Backfill zero-amount zaps by parsing bolt11 tags from corresponding events.
    /// This should be called once at startup to fix historical data.
    pub async fn backfill_zero_amount_zaps(&self) -> Result<i32, AppError> {
        tracing::info!("Starting backfill of zero-amount zaps...");
        
        let rows = sqlx::query(
            r#"
            SELECT zm.event_id, e.tags
            FROM zap_metadata zm
            JOIN events e ON zm.event_id = e.id
            WHERE zm.amount_msats = 0
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        let mut updated_count = 0;
        
        for row in rows {
            let event_id: String = row.get("event_id");
            let tags: serde_json::Value = row.get("tags");
            
            if let Some(tags_array) = tags.as_array() {
                // Find bolt11 tag
                for tag in tags_array {
                    if let Some(tag_array) = tag.as_array() {
                        if tag_array.len() >= 2 && tag_array[0].as_str() == Some("bolt11") {
                            if let Some(bolt11) = tag_array[1].as_str() {
                                if let Some(amount_msats) = self.parse_bolt11_amount(bolt11) {
                                    sqlx::query(
                                        "UPDATE zap_metadata SET amount_msats = $1 WHERE event_id = $2"
                                    )
                                    .bind(amount_msats)
                                    .bind(&event_id)
                                    .execute(&self.pool)
                                    .await?;
                                    
                                    updated_count += 1;
                                }
                                break;
                            }
                        }
                    }
                }
            }
        }
        
        tracing::info!("Backfill complete: updated {} zero-amount zaps", updated_count);
        Ok(updated_count)
    }

    /// Extract zap metadata from a kind-9735 zap receipt's embedded zap request.
    /// The "description" tag contains a JSON-encoded kind-9734 event whose tags
    /// include ["amount", "<msats>"]. We persist normalized zap metadata for fast profile queries.
    async fn extract_zap_metadata(&self, event: &NostrEvent) -> Result<(), AppError> {
        let description = event
            .tags
            .iter()
            .find(|t| t.len() >= 2 && t[0] == "description")
            .map(|t| &t[1]);

        let mut amount_msats: i64 = 0;
        let mut sender_pubkey: Option<String> = None;

        if let Some(desc_json) = description {
            if let Ok(zap_request) = serde_json::from_str::<serde_json::Value>(desc_json) {
                sender_pubkey = zap_request
                    .get("pubkey")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_lowercase());

                if let Some(tags) = zap_request.get("tags").and_then(|t| t.as_array()) {
                    for tag in tags {
                        let Some(arr) = tag.as_array() else { continue };
                        if arr.len() >= 2 && arr[0].as_str() == Some("amount") {
                            if let Some(raw) = arr[1].as_str() {
                                if let Ok(parsed) = raw.parse::<i64>() {
                                    amount_msats = parsed;
                                }
                                break;
                            }
                        }
                    }
                }
            }
        }

        // If amount is still 0, try to parse from bolt11 tag
        if amount_msats == 0 {
            if let Some(bolt11_tag) = event.tags.iter().find(|t| t.len() >= 2 && t[0] == "bolt11") {
                if let Some(bolt11) = bolt11_tag.get(1) {
                    if let Some(parsed_amount) = self.parse_bolt11_amount(bolt11) {
                        amount_msats = parsed_amount;
                    }
                }
            }
        }

        let recipient_pubkey = event
            .tags
            .iter()
            .find(|t| t.len() >= 2 && t[0] == "p")
            .and_then(|t| t.get(1))
            .map(|s| s.to_lowercase());

        let zapped_event_id = event
            .tags
            .iter()
            .find(|t| t.len() >= 2 && t[0] == "e")
            .and_then(|t| t.get(1))
            .cloned();

        sqlx::query(
            "INSERT INTO zap_metadata (event_id, sender_pubkey, recipient_pubkey, amount_msats, zapped_event_id, created_at)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (event_id) DO NOTHING",
        )
        .bind(&event.id)
        .bind(sender_pubkey)
        .bind(recipient_pubkey)
        .bind(amount_msats)
        .bind(zapped_event_id)
        .bind(event.created_at)
        .execute(&self.pool)
        .await?;

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

    /// Latest kind-0 (profile metadata) events for a list of pubkeys.
    /// Returns raw StoredEvents suitable for relay EVENT responses.
    pub async fn profile_events_for_pubkeys(
        &self,
        pubkeys: &[String],
    ) -> Result<Vec<StoredEvent>, AppError> {
        if pubkeys.is_empty() {
            return Ok(vec![]);
        }

        let rows = sqlx::query_as::<_, StoredEvent>(
            r#"
            SELECT DISTINCT ON (pubkey)
                id, pubkey, created_at, kind, content, sig, tags, raw, relay_url, received_at
            FROM events
            WHERE kind = 0 AND pubkey = ANY($1)
            ORDER BY pubkey, created_at DESC
            "#,
        )
        .bind(pubkeys)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    /// Fetch the latest events matching specific kinds and authors.
    ///
    /// For replaceable kinds (0, 3, 10002) returns only the latest per (pubkey, kind).
    /// Supports optional since/until timestamp filters. Results ordered by created_at DESC.
    pub async fn events_by_kinds_and_authors(
        &self,
        kinds: &[i32],
        authors: &[String],
        since: Option<i64>,
        until: Option<i64>,
        limit: i64,
    ) -> Result<Vec<StoredEvent>, AppError> {
        if authors.is_empty() || kinds.is_empty() {
            return Ok(vec![]);
        }

        // For replaceable event kinds we want DISTINCT ON (pubkey, kind)
        // to return only the latest version per author per kind.
        let mut sql = String::from(
            r#"SELECT DISTINCT ON (pubkey, kind)
                id, pubkey, created_at, kind, content, sig, tags, raw, relay_url, received_at
            FROM events
            WHERE kind = ANY($1) AND pubkey = ANY($2)"#,
        );

        let mut param_idx = 3u32;

        if since.is_some() {
            sql.push_str(&format!(" AND created_at >= ${param_idx}"));
            param_idx += 1;
        }
        if until.is_some() {
            sql.push_str(&format!(" AND created_at <= ${param_idx}"));
            // param_idx += 1; // not needed further
        }

        sql.push_str(" ORDER BY pubkey, kind, created_at DESC");
        sql.push_str(&format!(" LIMIT {limit}"));

        let mut query = sqlx::query_as::<_, StoredEvent>(&sql)
            .bind(kinds)
            .bind(authors);

        if let Some(s) = since {
            query = query.bind(s);
        }
        if let Some(u) = until {
            query = query.bind(u);
        }

        let rows = query.fetch_all(&self.pool).await?;
        Ok(rows)
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

    /// Count events matching the same filters as query_events (without limit/offset).
    pub async fn count_events_filtered(
        &self,
        pubkey: Option<&str>,
        kind: Option<i32>,
        since: Option<i64>,
        until: Option<i64>,
    ) -> Result<i64, AppError> {
        let mut sql = String::from("SELECT COUNT(*) FROM events WHERE 1=1");
        let mut param_idx = 1u32;

        let mut conditions = Vec::new();

        if pubkey.is_some() {
            conditions.push(format!("pubkey = ${param_idx}"));
            param_idx += 1;
        }
        if kind.is_some() {
            conditions.push(format!("kind = ${param_idx}"));
            param_idx += 1;
        }
        if since.is_some() {
            conditions.push(format!("created_at >= ${param_idx}"));
            param_idx += 1;
        }
        if until.is_some() {
            conditions.push(format!("created_at <= ${param_idx}"));
        }

        for cond in &conditions {
            sql.push_str(&format!(" AND {cond}"));
        }

        let mut query = sqlx::query_scalar::<_, i64>(&sql);

        if let Some(v) = pubkey {
            query = query.bind(v);
        }
        if let Some(v) = kind {
            query = query.bind(v);
        }
        if let Some(v) = since {
            query = query.bind(v);
        }
        if let Some(v) = until {
            query = query.bind(v);
        }

        let count = query.fetch_one(&self.pool).await?;
        Ok(count)
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
    /// v2: reads directly from counter columns on the events table.
    pub async fn get_interactions(&self, event_id: &str) -> Result<EventInteractions, AppError> {
        let row = sqlx::query_as::<_, (i32, i32, i32, i32, i64)>(
            r#"
            SELECT reaction_count, repost_count, reply_count, zap_count, zap_amount_msats
            FROM events WHERE id = $1
            "#,
        )
        .bind(event_id)
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some((reactions, reposts, replies, zaps, zap_msats)) => Ok(EventInteractions {
                replies: replies as i64,
                reactions: reactions as i64,
                reposts: reposts as i64,
                zaps: zaps as i64,
                zap_sats: zap_msats / 1000,
            }),
            None => Ok(EventInteractions {
                replies: 0,
                reactions: 0,
                reposts: 0,
                zaps: 0,
                zap_sats: 0,
            }),
        }
    }

    /// Batch-fetch engagement stats for multiple events by ID.
    /// v2: reads directly from counter columns.
    pub async fn batch_get_interactions(
        &self,
        event_ids: &[String],
    ) -> Result<std::collections::HashMap<String, EventInteractions>, AppError> {
        if event_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        let rows = sqlx::query(
            r#"
            SELECT id, reaction_count, repost_count, reply_count, zap_count, zap_amount_msats
            FROM events
            WHERE id = ANY($1)
            "#,
        )
        .bind(event_ids)
        .fetch_all(&self.pool)
        .await?;

        let mut map = std::collections::HashMap::new();
        for row in rows {
            let eid: String = row.try_get("id")?;
            let zap_msats: i64 = row.try_get("zap_amount_msats")?;
            map.insert(
                eid,
                EventInteractions {
                    replies: row.try_get::<i32, _>("reply_count")? as i64,
                    reactions: row.try_get::<i32, _>("reaction_count")? as i64,
                    reposts: row.try_get::<i32, _>("repost_count")? as i64,
                    zaps: row.try_get::<i32, _>("zap_count")? as i64,
                    zap_sats: zap_msats / 1000,
                },
            );
        }
        Ok(map)
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

    /// Get events that reference a target event, matching any of the given ref_types.
    pub async fn get_referencing_events_multi(
        &self,
        target_event_id: &str,
        ref_types: &[&str],
        limit: i64,
    ) -> Result<Vec<StoredEvent>, AppError> {
        let types: Vec<String> = ref_types.iter().map(|s| s.to_string()).collect();
        let events = sqlx::query_as::<_, StoredEvent>(
            "SELECT e.id, e.pubkey, e.created_at, e.kind, e.content, e.sig, e.tags, e.raw, e.relay_url, e.received_at
             FROM events e
             INNER JOIN event_refs r ON r.source_event_id = e.id
             WHERE r.target_event_id = $1 AND r.ref_type = ANY($2)
             ORDER BY e.created_at DESC
             LIMIT $3",
        )
        .bind(target_event_id)
        .bind(&types)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(events)
    }

    /// Get the full thread context for an event: parent chain, interactions, and reply events.
    /// v2: reactions and reposts are counts-only (empty vecs for API compat).
    /// Zaps are still returned as full events since they're always stored.
    pub async fn get_thread(
        &self,
        event_id: &str,
        limit: i64,
    ) -> Result<Option<EventThread>, AppError> {
        let event = match self.get_event_by_id(event_id).await? {
            Some(e) => e,
            None => return Ok(None),
        };

        // Run independent queries in parallel
        let (refs_result, interactions_result, replies_result, zaps_result) = tokio::join!(
            // Find root and parent from this event's outgoing refs
            sqlx::query_as::<_, EventRef>(
                "SELECT source_event_id, target_event_id, ref_type, relay_hint, created_at
                 FROM event_refs
                 WHERE source_event_id = $1 AND ref_type IN ('root', 'reply')",
            )
            .bind(event_id)
            .fetch_all(&self.pool),
            self.get_interactions(event_id),
            self.get_referencing_events_multi(event_id, &["reply", "root"], limit),
            self.get_referencing_events(event_id, "zap", limit),
        );

        let refs = refs_result?;
        let interactions = interactions_result?;
        let replies = replies_result?;
        let zaps = zaps_result?;

        let root_id = refs
            .iter()
            .find(|r| r.ref_type == "root")
            .map(|r| r.target_event_id.clone());
        let parent_id = refs
            .iter()
            .find(|r| r.ref_type == "reply")
            .map(|r| r.target_event_id.clone());

        Ok(Some(EventThread {
            event,
            root_id,
            parent_id,
            interactions,
            replies,
            reactions: vec![], // v2: counts-only, no individual reaction events stored
            reposts: vec![],   // v2: counts-only, no individual repost events stored
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
    /// Uses profile_search MV for followers_count (refreshed every 5min) to avoid
    /// expensive COUNT(*) on large follower sets.
    pub async fn follow_counts(&self, pubkey: &str) -> Result<(i64, i64), AppError> {
        let follows_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM follows WHERE follower_pubkey = $1")
                .bind(pubkey)
                .fetch_one(&self.pool)
                .await?;

        let followers_count: i64 = sqlx::query_scalar(
            "SELECT COALESCE(follower_count, 0) FROM profile_search WHERE pubkey = $1",
        )
        .bind(pubkey)
        .fetch_optional(&self.pool)
        .await?
        .unwrap_or(0);

        Ok((follows_count, followers_count))
    }



    /// Top notes ranked by a specific metric, using counter columns directly.
    /// v2: massive perf improvement -- no more subquery joins through event_refs.
    pub async fn top_notes_unified(
        &self,
        ref_type: &str,
        since: Option<i64>,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<RankedEvent>, Vec<ProfileRow>), AppError> {
        // Map API ref_type to counter column
        let order_col = match ref_type {
            "reaction" => "reaction_count",
            "repost" => "repost_count",
            "reply" => "reply_count",
            "zap" => "zap_amount_msats",
            _ => "reaction_count",
        };

        // Split into two query paths so Postgres can use the composite
        // indexes (idx_events_k1_created_{metric}) for time-bounded queries.
        // The OR pattern ($1 IS NULL OR created_at >= $1) prevents index usage.
        let rows = if let Some(since_ts) = since {
            let sql = format!(
                r#"
                SELECT
                    e.id, e.pubkey, e.created_at, e.kind, e.content, e.sig,
                    e.tags, e.raw, e.relay_url, e.received_at,
                    e.reaction_count::bigint AS reactions,
                    e.repost_count::bigint AS reposts,
                    e.reply_count::bigint AS replies,
                    e.zap_count::bigint AS zaps,
                    e.zap_amount_msats,
                    {order_col}::bigint AS metric_count
                FROM events e
                WHERE e.kind = 1
                  AND e.created_at >= $1
                  AND {order_col} > 0
                ORDER BY {order_col} DESC, e.created_at DESC
                LIMIT $2 OFFSET $3
                "#
            );
            sqlx::query(&sql)
                .bind(since_ts)
                .bind(limit * 4)
                .bind(offset)
                .fetch_all(&self.pool)
                .await?
        } else {
            let sql = format!(
                r#"
                SELECT
                    e.id, e.pubkey, e.created_at, e.kind, e.content, e.sig,
                    e.tags, e.raw, e.relay_url, e.received_at,
                    e.reaction_count::bigint AS reactions,
                    e.repost_count::bigint AS reposts,
                    e.reply_count::bigint AS replies,
                    e.zap_count::bigint AS zaps,
                    e.zap_amount_msats,
                    {order_col}::bigint AS metric_count
                FROM events e
                WHERE e.kind = 1
                  AND {order_col} > 0
                ORDER BY {order_col} DESC, e.created_at DESC
                LIMIT $1 OFFSET $2
                "#
            );
            sqlx::query(&sql)
                .bind(limit * 4)
                .bind(offset)
                .fetch_all(&self.pool)
                .await?
        };

        let is_zap = ref_type == "zap";

        let mut all_pubkeys = Vec::new();
        let all_events: Vec<RankedEvent> = rows
            .into_iter()
            .map(|row| -> Result<RankedEvent, sqlx::Error> {
                let pubkey: String = row.try_get("pubkey")?;
                all_pubkeys.push(pubkey.clone());
                let event = StoredEvent {
                    id: row.try_get("id")?,
                    pubkey,
                    created_at: row.try_get("created_at")?,
                    kind: row.try_get("kind")?,
                    content: row.try_get("content")?,
                    sig: row.try_get("sig")?,
                    tags: row.try_get("tags")?,
                    raw: row.try_get("raw")?,
                    relay_url: row.try_get("relay_url").ok(),
                    received_at: row.try_get("received_at")?,
                };
                let count: i64 = row.try_get("metric_count")?;
                let zap_msats: i64 = row.try_get("zap_amount_msats")?;
                let total_sats = if is_zap { Some(zap_msats / 1000) } else { None };
                Ok(RankedEvent {
                    event,
                    count,
                    total_sats,
                    reactions: row.try_get::<i64, _>("reactions").unwrap_or(0),
                    replies: row.try_get::<i64, _>("replies").unwrap_or(0),
                    reposts: row.try_get::<i64, _>("reposts").unwrap_or(0),
                    zap_sats: zap_msats / 1000,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        let passing = self.wot_cache.retain_passing(&all_pubkeys).await;
        let mut pubkeys = Vec::new();
        let events: Vec<RankedEvent> = all_events
            .into_iter()
            .filter(|e| passing.contains(&e.event.pubkey))
            .take(limit as usize)
            .inspect(|e| pubkeys.push(e.event.pubkey.clone()))
            .collect();

        let unique_pubkeys: Vec<String> = {
            let mut seen = HashSet::new();
            pubkeys
                .into_iter()
                .filter(|pk| seen.insert(pk.clone()))
                .collect()
        };
        let profiles = self.latest_profile_metadata(&unique_pubkeys).await?;

        Ok((events, profiles))
    }

    /// Trending notes: composite score combining zaps (1 point per sat), reposts (1000),
    /// replies (500), and reactions (100). 24h window.
    /// v2: reads directly from counter columns -- no event_refs joins needed.
    pub async fn trending_notes(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<TrendingNote>, AppError> {
        let since = chrono::Utc::now().timestamp() - 86400;

        // Two-step query: first score candidates via the covering index
        // (index-only scan on idx_events_trending_candidates), then fetch
        // full rows for only the top N candidates.
        let rows = sqlx::query(
            r#"
            WITH top AS (
                SELECT id, pubkey,
                    (zap_amount_msats / 1000)::bigint AS zap_sats,
                    repost_count::bigint AS repost_count,
                    reply_count::bigint AS reply_count,
                    reaction_count::bigint AS reaction_count,
                    (
                        zap_amount_msats / 1000
                        + repost_count * 1000
                        + reply_count * 500
                        + reaction_count * 100
                    )::bigint AS score,
                    created_at
                FROM events
                WHERE kind = 1
                  AND created_at >= $1
                  AND (reaction_count + repost_count + reply_count + zap_count) > 0
                ORDER BY (
                    zap_amount_msats / 1000
                    + repost_count * 1000
                    + reply_count * 500
                    + reaction_count * 100
                ) DESC, created_at DESC
                LIMIT $2 OFFSET $3
            )
            SELECT e.id, e.pubkey, e.created_at, e.kind, e.content, e.sig, e.tags, e.raw,
                   e.relay_url, e.received_at,
                   t.zap_sats, t.repost_count, t.reply_count, t.reaction_count, t.score
            FROM top t
            JOIN events e ON e.id = t.id
            ORDER BY t.score DESC, t.created_at DESC
            "#,
        )
        .bind(since)
        .bind(limit * 4)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        let all_notes = rows
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
            .collect::<Result<Vec<TrendingNote>, _>>()?;

        let all_pubkeys: Vec<String> = all_notes.iter().map(|n| n.event.pubkey.clone()).collect();
        let passing = self.wot_cache.retain_passing(&all_pubkeys).await;
        let notes: Vec<TrendingNote> = all_notes
            .into_iter()
            .filter(|n| passing.contains(&n.event.pubkey))
            .take(limit as usize)
            .collect();

        Ok(notes)
    }

    /// New users: pubkeys whose earliest event is within the last 24h.
    pub async fn new_users(&self, limit: i64, offset: i64) -> Result<Vec<NewUser>, AppError> {
        let since = chrono::Utc::now().timestamp() - 86400;

        let rows = sqlx::query_as::<_, (String, i64, i64)>(
            r#"
            SELECT pubkey, first_seen, event_count
            FROM mv_pubkey_first_seen
            WHERE first_seen >= $1
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

        // "Up and coming" users: most new followers in the last 24h,
        // but only accounts with fewer than 500 total followers.
        // Uses profile_search materialized view for fast follower_count
        // filtering instead of scanning the entire follows table.
        let rows = sqlx::query_as::<_, (String, i64)>(
            r#"
            SELECT f.followed_pubkey, COUNT(DISTINCT f.follower_pubkey) AS new_followers
            FROM follows f
            JOIN profile_search ps ON ps.pubkey = f.followed_pubkey AND ps.follower_count < 500
            WHERE f.created_at >= $1
            GROUP BY f.followed_pubkey
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

        // Total sats: sum of zap receipt amounts in last 24h.
        // zap_metadata only stores kind-9735 events, so no join with events needed.
        let total_sats: i64 = sqlx::query_scalar(
            r#"
            SELECT COALESCE(SUM(amount_msats) / 1000, 0)::bigint
            FROM zap_metadata
            WHERE created_at >= $1
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

    /// Daily zap sats only (last 24h). Uses the indexed `zap_metadata.created_at` column.
    /// ~5ms with the index vs minutes without.
    pub async fn daily_zap_sats(&self) -> Result<i64, AppError> {
        let since = chrono::Utc::now().timestamp() - 86400;
        let total_sats: i64 = sqlx::query_scalar(
            r#"
            SELECT COALESCE(SUM(amount_msats) / 1000, 0)::bigint
            FROM zap_metadata
            WHERE created_at >= $1
            "#,
        )
        .bind(since)
        .fetch_one(&self.pool)
        .await?;
        Ok(total_sats)
    }

    /// Top zappers: users ranked by total sats sent or received in the specified timeframe.
    pub async fn top_zappers(
        &self,
        direction: &str,
        range: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<super::models::TopZapper>, AppError> {
        let (sats_col, count_col) = match range {
            "today" => ("sats_today", "count_today"),
            "7d"    => ("sats_7d",    "count_7d"),
            "30d"   => ("sats_30d",   "count_30d"),
            _       => ("sats_all",   "count_all"),
        };

        let sql = format!(
            r#"
            SELECT pubkey, {sats_col} AS total_sats, {count_col} AS zap_count
            FROM mv_zapper_leaderboards
            WHERE direction = $1 AND {sats_col} > 0
            ORDER BY {sats_col} DESC
            LIMIT $2 OFFSET $3
            "#,
            sats_col = sats_col,
            count_col = count_col,
        );

        let rows = sqlx::query_as::<_, (String, i64, i64)>(&sql)
            .bind(direction)
            .bind(limit)
            .bind(offset)
            .fetch_all(&self.pool)
            .await?;

        Ok(rows
            .into_iter()
            .map(|(pubkey, total_sats, zap_count)| super::models::TopZapper {
                pubkey,
                total_sats,
                zap_count,
            })
            .collect())
    }

    /// Top posters: authors ranked by number of kind=1 notes published in the timeframe.
    pub async fn top_posters(
        &self,
        range: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<super::models::TopPoster>, AppError> {
        let col = match range {
            "today" => "note_count_today",
            "7d"    => "note_count_7d",
            "30d"   => "note_count_30d",
            _       => "note_count_all",
        };

        let sql = format!(
            r#"
            SELECT pubkey, {col} AS note_count
            FROM mv_author_leaderboards
            WHERE {col} > 0
            ORDER BY {col} DESC
            LIMIT $1 OFFSET $2
            "#,
            col = col,
        );

        let rows = sqlx::query_as::<_, (String, i64)>(&sql)
            .bind(limit)
            .bind(offset)
            .fetch_all(&self.pool)
            .await?;

        Ok(rows
            .into_iter()
            .map(|(pubkey, note_count)| super::models::TopPoster {
                pubkey,
                note_count,
            })
            .collect())
    }

    /// Most liked authors: authors whose kind=1 notes have the highest reaction_count.
    /// Reactions (kind=7) are stored as counter increments on the note, not as separate events.
    pub async fn most_liked_authors(
        &self,
        range: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<super::models::MostLikedAuthor>, AppError> {
        let col = match range {
            "today" => "like_count_today",
            "7d"    => "like_count_7d",
            "30d"   => "like_count_30d",
            _       => "like_count_all",
        };

        let sql = format!(
            r#"
            SELECT pubkey, {col} AS like_count
            FROM mv_author_leaderboards
            WHERE {col} > 0
            ORDER BY {col} DESC
            LIMIT $1 OFFSET $2
            "#,
            col = col,
        );

        let rows = sqlx::query_as::<_, (String, i64)>(&sql)
            .bind(limit)
            .bind(offset)
            .fetch_all(&self.pool)
            .await?;

        Ok(rows
            .into_iter()
            .map(|(pubkey, like_count)| super::models::MostLikedAuthor {
                pubkey,
                like_count,
            })
            .collect())
    }

    /// Most shared authors: authors whose kind=1 notes have the highest repost_count.
    /// Reposts (kind=6/16) are stored as counter increments on the note, not as separate events.
    pub async fn most_shared_authors(
        &self,
        range: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<super::models::MostSharedAuthor>, AppError> {
        let col = match range {
            "today" => "repost_count_today",
            "7d"    => "repost_count_7d",
            "30d"   => "repost_count_30d",
            _       => "repost_count_all",
        };

        let sql = format!(
            r#"
            SELECT pubkey, {col} AS repost_count
            FROM mv_author_leaderboards
            WHERE {col} > 0
            ORDER BY {col} DESC
            LIMIT $1 OFFSET $2
            "#,
            col = col,
        );

        let rows = sqlx::query_as::<_, (String, i64)>(&sql)
            .bind(limit)
            .bind(offset)
            .fetch_all(&self.pool)
            .await?;

        Ok(rows
            .into_iter()
            .map(|(pubkey, repost_count)| super::models::MostSharedAuthor {
                pubkey,
                repost_count,
            })
            .collect())
    }

    /// Search profiles with ranked results.
    ///
    /// Ranking algorithm (rebalanced so follower/engagement weight is
    /// meaningful relative to match-quality bonuses):
    /// - Exact name match: +500
    /// - Exact NIP-05 match: +400
    /// - Prefix match: +200
    /// - NIP-05 prefix match: +100
    /// - Trigram similarity: 0-100
    /// - Follower influence: ln(followers + 1) * 100
    /// - Engagement influence: ln(engagement + 1) * 50
    /// - Recency bonus: +200 if active in last 7d, +100 if last 30d
    #[allow(dead_code)]
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
                    CASE WHEN LOWER(name) = LOWER($1) THEN 500 ELSE 0 END +
                    CASE WHEN LOWER(display_name) = LOWER($1) THEN 500 ELSE 0 END +
                    CASE WHEN LOWER(nip05) = LOWER($1) THEN 400 ELSE 0 END +
                    CASE WHEN name ILIKE $1 || '%' THEN 200 ELSE 0 END +
                    CASE WHEN display_name ILIKE $1 || '%' THEN 200 ELSE 0 END +
                    CASE WHEN nip05 ILIKE $1 || '%' THEN 100 ELSE 0 END +
                    GREATEST(
                        COALESCE(similarity(name, $1), 0),
                        COALESCE(similarity(display_name, $1), 0)
                    ) * 100 +
                    LN(GREATEST(follower_count, 0) + 1) * 100 +
                    LN(GREATEST(engagement_score, 0) + 1) * 50 +
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
    #[allow(dead_code)]
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
                        WHEN LOWER(name) = LOWER($1) OR LOWER(display_name) = LOWER($1) THEN 500
                        WHEN name ILIKE $1 || '%' OR display_name ILIKE $1 || '%' THEN 200
                        WHEN nip05 ILIKE $1 || '%' THEN 100
                        ELSE 0
                    END
                    + LN(GREATEST(follower_count, 0) + 1) * 100
                    + LN(GREATEST(engagement_score, 0) + 1) * 50
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
    /// Ranks against lightweight search_index table, joins back to events for full data.
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
                    s.event_id, s.created_at,
                    s.reaction_count, s.reply_count, s.repost_count, s.zap_count,
                    ts_rank(s.content_tsv, query) AS text_rank
                FROM search_index s, plainto_tsquery('english', $1) query
                WHERE s.content_tsv @@ query
                ORDER BY ts_rank(s.content_tsv, query) DESC
                LIMIT 200
            )
            SELECT
                e.id, e.pubkey, e.created_at, e.kind, e.content, e.sig,
                e.tags, e.raw, e.relay_url, e.received_at,
                r.reaction_count::bigint AS reactions,
                r.reply_count::bigint AS replies,
                r.repost_count::bigint AS reposts,
                r.zap_count::bigint AS zaps,
                (
                    r.text_rank * 1000 +
                    LN(
                        r.reaction_count * 100 +
                        r.reply_count * 500 +
                        r.repost_count * 1000 +
                        r.zap_count * 2000 + 1
                    ) * 10 +
                    CASE
                        WHEN r.created_at > EXTRACT(EPOCH FROM NOW())::bigint - 86400 THEN 50
                        WHEN r.created_at > EXTRACT(EPOCH FROM NOW())::bigint - 604800 THEN 25
                        ELSE 0
                    END
                )::float8 AS rank_score
            FROM ranked r
            JOIN events e ON e.id = r.event_id
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

    /// Fetch everything the note detail page needs in a single SQL round-trip.
    ///
    /// Returns a JSON object with: event, root_id, parent_id, stats, replies, profiles.
    /// Uses CTEs so Postgres does all the heavy lifting in one query plan.
    /// Fetch everything the note detail page needs.
    /// v2: stats come from counter columns, no event_refs aggregate for stats.
    pub async fn get_note_detail(
        &self,
        event_id: &str,
        reply_limit: i64,
    ) -> Result<Option<serde_json::Value>, AppError> {
        let row: (serde_json::Value,) = sqlx::query_as(
            r#"
            WITH target AS (
                SELECT id, pubkey, created_at, kind, content, sig, tags,
                       relay_url, received_at,
                       reaction_count, repost_count, reply_count, zap_count
                FROM events
                WHERE id = $1
            ),
            thread_refs AS (
                SELECT
                    MAX(CASE WHEN ref_type = 'root'  THEN target_event_id END) AS root_id,
                    MAX(CASE WHEN ref_type = 'reply'  THEN target_event_id END) AS parent_id
                FROM event_refs
                WHERE source_event_id = $1
                  AND ref_type IN ('root', 'reply')
            ),
            reply_events AS (
                SELECT e.id, e.pubkey, e.created_at, e.kind, e.content,
                       e.sig, e.tags, e.relay_url, e.received_at
                FROM events e
                INNER JOIN event_refs r ON r.source_event_id = e.id
                WHERE r.target_event_id = $1 AND r.ref_type IN ('reply', 'root')
                ORDER BY e.created_at DESC
                LIMIT $2
            ),
            all_pubkeys AS (
                SELECT pubkey FROM target
                UNION
                SELECT pubkey FROM reply_events
            ),
            profiles AS (
                SELECT DISTINCT ON (e.pubkey) e.pubkey,
                    CASE WHEN e.content ~ '^\s*\{'
                        THEN json_build_object(
                            'name',         (e.content::jsonb)->>'name',
                            'display_name', (e.content::jsonb)->>'display_name',
                            'picture',      (e.content::jsonb)->>'picture',
                            'nip05',        (e.content::jsonb)->>'nip05'
                        )
                        ELSE json_build_object(
                            'name', NULL, 'display_name', NULL,
                            'picture', NULL, 'nip05', NULL
                        )
                    END AS metadata
                FROM events e
                INNER JOIN all_pubkeys ap ON e.pubkey = ap.pubkey
                WHERE e.kind = 0
                ORDER BY e.pubkey, e.created_at DESC
            )
            SELECT json_build_object(
                'event',     (SELECT row_to_json(t)   FROM target t),
                'root_id',   (SELECT root_id          FROM thread_refs),
                'parent_id', (SELECT parent_id        FROM thread_refs),
                'stats',     (SELECT json_build_object(
                                 'replies',   reply_count,
                                 'reactions', reaction_count,
                                 'reposts',  repost_count,
                                 'zaps',     zap_count
                             ) FROM target),
                'replies',   COALESCE(
                    (SELECT json_agg(row_to_json(re) ORDER BY re.created_at DESC)
                     FROM reply_events re), '[]'::json
                ),
                'profiles',  COALESCE(
                    (SELECT json_object_agg(p.pubkey, p.metadata)
                     FROM profiles p), '{}'::json
                )
            ) AS result
            "#,
        )
        .bind(event_id)
        .bind(reply_limit)
        .fetch_one(&self.pool)
        .await?;

        if row.0.get("event").map_or(true, |v| v.is_null()) {
            return Ok(None);
        }

        Ok(Some(row.0))
    }

    /// Advanced note search with full-text search, exclusions, author/reply_to filters,
    /// and multiple ordering modes (newest, oldest, engagement).
    ///
    /// Returns (notes, total_count, profiles).
    pub async fn advanced_search_notes(
        &self,
        q: Option<&str>,
        exclude: Option<&str>,
        author: Option<&str>,
        reply_to: Option<&str>,
        order: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<super::models::AdvancedNoteSearchEntry>, i64, Vec<ProfileRow>), AppError> {
        let mut param_idx = 1u32;
        let mut conditions = Vec::new();

        // Track bind values in order
        let mut bind_values: Vec<BindValue> = Vec::new();

        // Always filter kind = 1
        conditions.push("e.kind = 1".to_string());

        // Full-text search (q) — plainto_tsquery safely handles arbitrary user input
        if let Some(query) = q {
            let trimmed = query.trim();
            if !trimmed.is_empty() {
                conditions.push(format!(
                    "e.content_tsv @@ plainto_tsquery('english', ${param_idx})"
                ));
                bind_values.push(BindValue::Text(trimmed.to_string()));
                param_idx += 1;
            }
        }

        // Exclude words — plainto_tsquery safely handles arbitrary user input
        if let Some(excl) = exclude {
            let trimmed = excl.trim();
            if !trimmed.is_empty() {
                conditions.push(format!(
                    "NOT e.content_tsv @@ plainto_tsquery('english', ${param_idx})"
                ));
                bind_values.push(BindValue::Text(trimmed.to_string()));
                param_idx += 1;
            }
        }

        // Author filter
        if let Some(author_pk) = author {
            conditions.push(format!("e.pubkey = ${param_idx}"));
            bind_values.push(BindValue::Text(author_pk.to_string()));
            param_idx += 1;
        }

        // Reply-to filter
        if let Some(reply_pk) = reply_to {
            conditions.push(format!(
                "EXISTS (SELECT 1 FROM event_refs er JOIN events parent ON parent.id = er.target_event_id WHERE er.source_event_id = e.id AND er.ref_type IN ('reply', 'root') AND parent.pubkey = ${param_idx})"
            ));
            bind_values.push(BindValue::Text(reply_pk.to_string()));
            param_idx += 1;
        }

        let where_clause = conditions.join(" AND ");

        // Build count query
        let count_sql = format!("SELECT COUNT(*) FROM events e WHERE {where_clause}");

        // v2: engagement stats from counter columns directly, no LATERAL join needed
        let order_clause = match order {
            "oldest" => "e.created_at ASC",
            "engagement" => "(e.reaction_count * 1 + e.reply_count * 5 + e.repost_count * 10 + e.zap_count * 20) DESC, e.created_at DESC",
            _ => "e.created_at DESC", // newest (default)
        };

        let data_sql = format!(
            r#"SELECT
                e.id, e.pubkey, e.created_at, e.kind, e.content, e.sig,
                e.tags, e.raw, e.relay_url, e.received_at,
                e.reaction_count::bigint AS reactions,
                e.reply_count::bigint AS replies,
                e.repost_count::bigint AS reposts,
                (e.zap_amount_msats / 1000)::bigint AS zap_sats
            FROM events e
            WHERE {where_clause}
            ORDER BY {order_clause}
            LIMIT ${param_idx} OFFSET ${}"#,
            param_idx + 1
        );

        // Execute count query
        let mut count_query = sqlx::query_scalar::<_, i64>(&count_sql);
        for val in &bind_values {
            match val {
                BindValue::Text(v) => count_query = count_query.bind(v),
            }
        }
        let total = count_query.fetch_one(&self.pool).await?;

        // Execute data query
        let mut data_query = sqlx::query(&data_sql);
        for val in &bind_values {
            match val {
                BindValue::Text(v) => data_query = data_query.bind(v),
            }
        }
        data_query = data_query.bind(limit).bind(offset);

        let rows = data_query.fetch_all(&self.pool).await?;

        let mut pubkeys_set = HashSet::new();
        let entries: Vec<super::models::AdvancedNoteSearchEntry> = rows
            .into_iter()
            .map(|row| -> Result<super::models::AdvancedNoteSearchEntry, sqlx::Error> {
                let pubkey: String = row.try_get("pubkey")?;
                pubkeys_set.insert(pubkey.clone());
                let event = StoredEvent {
                    id: row.try_get("id")?,
                    pubkey,
                    created_at: row.try_get("created_at")?,
                    kind: row.try_get("kind")?,
                    content: row.try_get("content")?,
                    sig: row.try_get("sig")?,
                    tags: row.try_get("tags")?,
                    raw: row.try_get("raw")?,
                    relay_url: row.try_get("relay_url").ok(),
                    received_at: row.try_get("received_at")?,
                };
                Ok(super::models::AdvancedNoteSearchEntry {
                    event,
                    reactions: row.try_get("reactions")?,
                    replies: row.try_get("replies")?,
                    reposts: row.try_get("reposts")?,
                    zap_sats: row.try_get("zap_sats")?,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        let unique_pubkeys: Vec<String> = pubkeys_set.into_iter().collect();
        let profiles = self.latest_profile_metadata(&unique_pubkeys).await?;

        Ok((entries, total, profiles))
    }

    /// Search profiles and return raw kind-0 events, ranked by the profile search algorithm.
    /// Used by the NIP-50 WebSocket search relay.
    pub async fn search_profiles_as_events(
        &self,
        query: &str,
        limit: i64,
    ) -> Result<Vec<StoredEvent>, AppError> {
        let rows = sqlx::query_as::<_, StoredEvent>(
            r#"
            WITH ranked_profiles AS (
                SELECT
                    pubkey,
                    ROW_NUMBER() OVER (ORDER BY
                        (
                            CASE
                                WHEN LOWER(name) = LOWER($1) OR LOWER(display_name) = LOWER($1) THEN 500
                                WHEN name ILIKE $1 || '%' OR display_name ILIKE $1 || '%' THEN 200
                                WHEN nip05 ILIKE $1 || '%' THEN 100
                                ELSE 0
                            END
                            + LN(GREATEST(follower_count, 0) + 1) * 100
                            + LN(GREATEST(engagement_score, 0) + 1) * 50
                        ) DESC, follower_count DESC
                    ) AS rn
                FROM profile_search
                WHERE
                    name ILIKE '%' || $1 || '%'
                    OR display_name ILIKE '%' || $1 || '%'
                    OR nip05 ILIKE '%' || $1 || '%'
                    OR pubkey LIKE $1 || '%'
                LIMIT $2
            ),
            latest_profiles AS (
                SELECT DISTINCT ON (e.pubkey)
                    e.id, e.pubkey, e.created_at, e.kind, e.content, e.sig,
                    e.tags, e.raw, e.relay_url, e.received_at, rp.rn
                FROM ranked_profiles rp
                JOIN events e ON e.pubkey = rp.pubkey AND e.kind = 0
                ORDER BY e.pubkey, e.created_at DESC
            )
            SELECT id, pubkey, created_at, kind, content, sig, tags, raw, relay_url, received_at
            FROM latest_profiles
            ORDER BY rn
            "#,
        )
        .bind(query)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    /// Search notes and return raw kind-1 events, ranked by FTS relevance + engagement.
    /// Used by the NIP-50 WebSocket search relay.
    /// Ranks against lightweight search_index table, joins back to events for full data.
    pub async fn search_notes_as_events(
        &self,
        query: &str,
        limit: i64,
        authors: &[&str],
    ) -> Result<Vec<StoredEvent>, AppError> {
        if authors.is_empty() {
            let rows = sqlx::query_as::<_, StoredEvent>(
                r#"
                WITH candidates AS (
                    SELECT
                        s.event_id,
                        s.created_at,
                        s.content_tsv,
                        s.reaction_count, s.reply_count, s.repost_count, s.zap_count
                    FROM search_index s
                    WHERE s.content_tsv @@ plainto_tsquery('english', $1)
                    ORDER BY s.created_at DESC
                    LIMIT 500
                ),
                ranked AS (
                    SELECT
                        c.event_id,
                        c.created_at,
                        c.reaction_count, c.reply_count, c.repost_count, c.zap_count,
                        ts_rank(c.content_tsv, plainto_tsquery('english', $1)) AS text_rank
                    FROM candidates c
                )
                SELECT
                    e.id, e.pubkey, e.created_at, e.kind, e.content, e.sig,
                    e.tags, e.raw, e.relay_url, e.received_at
                FROM ranked r
                JOIN events e ON e.id = r.event_id
                ORDER BY (
                    r.text_rank * 1000 +
                    LN(
                        r.reaction_count * 100 +
                        r.reply_count * 500 +
                        r.repost_count * 1000 +
                        r.zap_count * 2000 + 1
                    ) * 10 +
                    CASE
                        WHEN r.created_at > EXTRACT(EPOCH FROM NOW())::bigint - 86400 THEN 50
                        WHEN r.created_at > EXTRACT(EPOCH FROM NOW())::bigint - 604800 THEN 25
                        ELSE 0
                    END
                ) DESC
                LIMIT $2
                "#,
            )
            .bind(query)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;

            Ok(rows)
        } else {
            let rows = sqlx::query_as::<_, StoredEvent>(
                r#"
                WITH candidates AS (
                    SELECT
                        s.event_id,
                        s.created_at,
                        s.content_tsv,
                        s.reaction_count, s.reply_count, s.repost_count, s.zap_count
                    FROM search_index s
                    WHERE s.content_tsv @@ plainto_tsquery('english', $1)
                      AND s.pubkey = ANY($3)
                    ORDER BY s.created_at DESC
                    LIMIT 500
                ),
                ranked AS (
                    SELECT
                        c.event_id,
                        c.created_at,
                        c.reaction_count, c.reply_count, c.repost_count, c.zap_count,
                        ts_rank(c.content_tsv, plainto_tsquery('english', $1)) AS text_rank
                    FROM candidates c
                )
                SELECT
                    e.id, e.pubkey, e.created_at, e.kind, e.content, e.sig,
                    e.tags, e.raw, e.relay_url, e.received_at
                FROM ranked r
                JOIN events e ON e.id = r.event_id
                ORDER BY (
                    r.text_rank * 1000 +
                    LN(
                        r.reaction_count * 100 +
                        r.reply_count * 500 +
                        r.repost_count * 1000 +
                        r.zap_count * 2000 + 1
                    ) * 10 +
                    CASE
                        WHEN r.created_at > EXTRACT(EPOCH FROM NOW())::bigint - 86400 THEN 50
                        WHEN r.created_at > EXTRACT(EPOCH FROM NOW())::bigint - 604800 THEN 25
                        ELSE 0
                    END
                ) DESC
                LIMIT $2
                "#,
            )
            .bind(query)
            .bind(limit)
            .bind(authors)
            .fetch_all(&self.pool)
            .await?;

            Ok(rows)
        }
    }

    /// Search kind-1 notes containing a literal hashtag (e.g. `#bitcoin`) in content.
    /// Case-insensitive via regex with word boundaries, uses GIN trigram index.
    ///
    /// Matches the exact hashtag — `#bitcoin` won't match `#bitcoinart`.
    /// This catches all notes regardless of whether the client added a `t` tag,
    /// since many clients get tagging wrong.
    ///
    /// Ranked by recency (newest first). Filters out spam by requiring the
    /// author to have at least 3 followers — cheap credibility check that
    /// eliminates bots and throwaway accounts without a heavy engagement join.
    /// Returns notes with engagement stats, ordered by engagement score then recency.
    pub async fn notes_by_hashtag(
        &self,
        hashtag: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<TrendingNote>, Vec<ProfileRow>), AppError> {
        self.notes_by_hashtags(&[hashtag.to_string()], limit, offset).await
    }

    /// Fetch notes matching ANY of the given hashtags, ranked by engagement score.
    /// Implements NIP-01 `#t` tag filter semantics (OR across values).
    /// Uses note_hashtags btree index for fast lookups, limits before joining to events.
    pub async fn notes_by_hashtags(
        &self,
        hashtags: &[String],
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<TrendingNote>, Vec<ProfileRow>), AppError> {
        let tags_lower: Vec<String> = hashtags
            .iter()
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty())
            .collect();

        if tags_lower.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }

        // Limit to last 7 days for popular hashtags.
        let since = chrono::Utc::now().timestamp() - 7 * 86400;

        let rows = sqlx::query(
            r#"
            WITH recent_ids AS (
                SELECT event_id, MAX(created_at) AS created_at
                FROM note_hashtags
                WHERE hashtag = ANY($1)
                  AND created_at >= $2
                GROUP BY event_id
                ORDER BY created_at DESC
                LIMIT 200
            )
            SELECT
                e.id, e.pubkey, e.created_at, e.kind, e.content, e.sig,
                e.tags, e.raw, e.relay_url, e.received_at,
                (e.zap_amount_msats / 1000)::bigint AS zap_sats,
                e.repost_count::bigint AS repost_count,
                e.reply_count::bigint AS reply_count,
                e.reaction_count::bigint AS reaction_count,
                (
                    e.zap_amount_msats / 1000
                    + e.repost_count * 1000
                    + e.reply_count * 500
                    + e.reaction_count * 100
                )::bigint AS score
            FROM recent_ids m
            JOIN events e ON e.id = m.event_id
            ORDER BY score DESC, e.created_at DESC
            LIMIT $3 OFFSET $4
            "#,
        )
        .bind(&tags_lower)
        .bind(since)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        let mut pubkeys = Vec::new();
        let notes = rows
            .into_iter()
            .map(|row| -> Result<TrendingNote, sqlx::Error> {
                let pubkey: String = row.try_get("pubkey")?;
                pubkeys.push(pubkey.clone());
                let event = StoredEvent {
                    id: row.try_get("id")?,
                    pubkey,
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

        let unique_pubkeys: Vec<String> = {
            let mut seen = HashSet::new();
            pubkeys.into_iter().filter(|pk| seen.insert(pk.clone())).collect()
        };
        let profiles = self.latest_profile_metadata(&unique_pubkeys).await?;

        Ok((notes, profiles))
    }

    /// Refresh the profile_search materialized view (CONCURRENTLY to avoid blocking reads).
    pub async fn refresh_profile_search(&self) -> Result<(), AppError> {
        sqlx::query("REFRESH MATERIALIZED VIEW CONCURRENTLY profile_search")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Refresh the analytics materialized views (client leaderboard, relay leaderboard, pubkey first-seen,
    /// author leaderboards, zapper leaderboards).
    /// These are heavy aggregations that should only run periodically (every 30-60 min).
    pub async fn refresh_analytics_views(&self) -> Result<(), AppError> {
        sqlx::query("REFRESH MATERIALIZED VIEW CONCURRENTLY mv_client_leaderboard")
            .execute(&self.pool)
            .await?;
        tracing::info!("refreshed mv_client_leaderboard");

        sqlx::query("REFRESH MATERIALIZED VIEW CONCURRENTLY mv_relay_leaderboard")
            .execute(&self.pool)
            .await?;
        tracing::info!("refreshed mv_relay_leaderboard");

        sqlx::query("REFRESH MATERIALIZED VIEW CONCURRENTLY mv_pubkey_first_seen")
            .execute(&self.pool)
            .await?;
        tracing::info!("refreshed mv_pubkey_first_seen");

        sqlx::query("REFRESH MATERIALIZED VIEW CONCURRENTLY mv_author_leaderboards")
            .execute(&self.pool)
            .await?;
        tracing::info!("refreshed mv_author_leaderboards");

        sqlx::query("REFRESH MATERIALIZED VIEW CONCURRENTLY mv_zapper_leaderboards")
            .execute(&self.pool)
            .await?;
        tracing::info!("refreshed mv_zapper_leaderboards");

        sqlx::query("REFRESH MATERIALIZED VIEW CONCURRENTLY mv_client_top_users")
            .execute(&self.pool)
            .await?;
        tracing::info!("refreshed mv_client_top_users");

        Ok(())
    }

    /// Get trending hashtags from kind-1 notes in the last 24 hours.
    /// Uses note_hashtags btree index instead of jsonb_array_elements scan.
    pub async fn trending_hashtags(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<super::models::TrendingHashtag>, AppError> {
        let since = chrono::Utc::now().timestamp() - 86400;

        // Overfetch to compensate for blocked hashtags being filtered out
        let fetch_limit = limit + 50;

        let rows = sqlx::query_as::<_, (String, i64)>(
            r#"
            SELECT nh.hashtag,
                   COUNT(DISTINCT nh.event_id)::bigint AS cnt
            FROM note_hashtags nh
            WHERE nh.created_at >= $1
            GROUP BY nh.hashtag
            HAVING COUNT(DISTINCT nh.event_id) >= 3
            ORDER BY cnt DESC
            LIMIT $2
            "#,
        )
        .bind(since)
        .bind(fetch_limit)
        .fetch_all(&self.pool)
        .await?;

        let blocked: HashSet<String> = self.block_cache.blocked_hashtags_snapshot().await;

        Ok(rows
            .into_iter()
            .filter(|(hashtag, _)| !blocked.contains(&hashtag.to_lowercase()))
            .skip(offset as usize)
            .take(limit as usize)
            .map(|(hashtag, count)| super::models::TrendingHashtag { hashtag, count })
            .collect())
    }

    /// Client leaderboard: top Nostr clients by note count and distinct users.
    ///
    /// Reads `client` tags from JSONB `events.tags`, joins to `events` (kind=1 notes only),
    /// and aggregates per client name. Case-insensitive grouping merges variants
    /// like "Coracle" / "coracle". Only counts notes from qualified users
    /// (at least 1 follower in `profile_search`) to filter out bot spam.
    /// Returns results ordered by note_count DESC.
    pub async fn client_leaderboard(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<super::models::ClientEntry>, AppError> {
        let rows = sqlx::query_as::<_, (String, i64, i64)>(
            r#"
            SELECT client_name, note_count, user_count
            FROM mv_client_leaderboard
            ORDER BY note_count DESC
            LIMIT $1 OFFSET $2
            "#,
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(client_name, note_count, user_count)| super::models::ClientEntry {
                client_name,
                note_count,
                user_count,
            })
            .collect())
    }

    /// Top users for a specific client, from `mv_client_top_users`.
    pub async fn client_users(
        &self,
        client_name: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<super::models::ClientUserEntry>, AppError> {
        let rows = sqlx::query_as::<_, (String, i64, i64, i64)>(
            r#"
            SELECT pubkey, note_count, first_seen, last_seen
            FROM mv_client_top_users
            WHERE client_name = $1
            ORDER BY note_count DESC
            LIMIT $2 OFFSET $3
            "#,
        )
        .bind(client_name.to_lowercase())
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(pubkey, note_count, first_seen, last_seen)| super::models::ClientUserEntry {
                pubkey,
                note_count,
                first_seen,
                last_seen,
            })
            .collect())
    }

    /// Top relays by number of users who list them in kind-10002 relay lists (NIP-65).
    /// Only counts the latest relay list per pubkey.
    pub async fn relay_leaderboard(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<super::models::RelayLeaderboardEntry>, AppError> {
        let rows = sqlx::query_as::<_, (String, i64)>(
            r#"
            SELECT relay_url, user_count
            FROM mv_relay_leaderboard
            ORDER BY user_count DESC
            LIMIT $1 OFFSET $2
            "#,
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(relay_url, user_count)| super::models::RelayLeaderboardEntry {
                relay_url,
                user_count,
            })
            .collect())
    }

    // ─── Daily Analytics ─────────────────────────────────────────────

    /// Compute and upsert daily analytics for a specific date.
    pub async fn compute_daily_analytics(
        &self,
        date: chrono::NaiveDate,
    ) -> Result<(), AppError> {
        let start_ts = date
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp();
        let end_ts = start_ts + 86400;

        // Events are already WoT-filtered at ingestion time, so no need for
        // a credible_actors join here — just query events directly.
        sqlx::query(
            r#"
            WITH day_events AS (
                SELECT pubkey, kind
                FROM events
                WHERE created_at >= $2 AND created_at < $3
            ),
            zap_sats AS (
                SELECT COALESCE(SUM(amount_msats) / 1000, 0)::bigint AS total_sats
                FROM zap_metadata
                WHERE created_at >= $2 AND created_at < $3
            )
            INSERT INTO daily_analytics (date, active_users, zaps_sent, notes_posted, computed_at)
            SELECT
                $1::date,
                (SELECT COUNT(DISTINCT pubkey) FROM day_events)::bigint,
                (SELECT total_sats FROM zap_sats),
                (SELECT COUNT(*) FROM day_events WHERE kind = 1)::bigint,
                NOW()
            ON CONFLICT (date) DO UPDATE SET
                active_users = EXCLUDED.active_users,
                zaps_sent = EXCLUDED.zaps_sent,
                notes_posted = EXCLUDED.notes_posted,
                computed_at = EXCLUDED.computed_at
            "#,
        )
        .bind(date)
        .bind(start_ts)
        .bind(end_ts)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Fetch daily analytics rows between two dates inclusive, ordered by date ASC.
    pub async fn get_daily_analytics(
        &self,
        since: chrono::NaiveDate,
        until: chrono::NaiveDate,
    ) -> Result<Vec<super::models::DailyAnalyticsRow>, AppError> {
        let rows = sqlx::query_as::<_, super::models::DailyAnalyticsRow>(
            "SELECT date, active_users, zaps_sent, notes_posted, computed_at
             FROM daily_analytics
             WHERE date >= $1 AND date <= $2
             ORDER BY date ASC",
        )
        .bind(since)
        .bind(until)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    /// Backfill daily analytics for the last N days, skipping dates that already have data.
    /// Returns the count of days computed.
    pub async fn backfill_daily_analytics(&self, days: i64) -> Result<i64, AppError> {
        let today = chrono::Utc::now().date_naive();
        let mut computed = 0i64;

        for i in 1..=days {
            let date = today - chrono::Duration::days(i);

            // Skip if already computed
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM daily_analytics WHERE date = $1)",
            )
            .bind(date)
            .fetch_one(&self.pool)
            .await?;

            if exists {
                continue;
            }

            self.compute_daily_analytics(date).await?;
            computed += 1;
        }

        Ok(computed)
    }

    // ─── Profile Tabs ───────────────────────────────────────────────

    /// Profile notes: kind 1 events by pubkey that are NOT replies.
    pub async fn profile_notes(
        &self,
        pubkey: &str,
        limit: i64,
        offset: i64,
        sort: &str,
    ) -> Result<Vec<StoredEvent>, AppError> {
        let order_clause = match sort {
            "likes" => "ORDER BY reaction_count DESC, created_at DESC",
            "zaps" => "ORDER BY zap_amount_msats DESC, created_at DESC",
            "reposts" => "ORDER BY repost_count DESC, created_at DESC",
            _ => "ORDER BY created_at DESC", // "recent" or default
        };

        let query = format!(
            r#"
            SELECT id, pubkey, created_at, kind, content, sig, tags, raw, relay_url, received_at
            FROM events
            WHERE pubkey = $1 AND kind = 1 AND NOT is_reply
            {}
            LIMIT $2 OFFSET $3
            "#,
            order_clause
        );

        let events = sqlx::query_as::<_, StoredEvent>(&query)
            .bind(pubkey)
            .bind(limit)
            .bind(offset)
            .fetch_all(&self.pool)
            .await?;

        Ok(events)
    }

    /// Profile replies: kind 1 events by pubkey that ARE replies.
    pub async fn profile_replies(
        &self,
        pubkey: &str,
        limit: i64,
        offset: i64,
        sort: &str,
    ) -> Result<Vec<StoredEvent>, AppError> {
        let order_clause = match sort {
            "likes" => "ORDER BY reaction_count DESC, created_at DESC",
            "zaps" => "ORDER BY zap_amount_msats DESC, created_at DESC",
            "reposts" => "ORDER BY repost_count DESC, created_at DESC",
            _ => "ORDER BY created_at DESC", // "recent" or default
        };

        let query = format!(
            r#"
            SELECT id, pubkey, created_at, kind, content, sig, tags, raw, relay_url, received_at
            FROM events
            WHERE pubkey = $1 AND kind = 1 AND is_reply
            {}
            LIMIT $2 OFFSET $3
            "#,
            order_clause
        );

        let events = sqlx::query_as::<_, StoredEvent>(&query)
            .bind(pubkey)
            .bind(limit)
            .bind(offset)
            .fetch_all(&self.pool)
            .await?;

        Ok(events)
    }

    /// Zaps sent by a pubkey (sender is in the embedded zap request).
    pub async fn profile_zaps_sent(
        &self,
        pubkey: &str,
        limit: i64,
        offset: i64,
        sort: &str,
    ) -> Result<(Vec<super::models::ProfileZapEntry>, i64, Vec<ProfileRow>), AppError> {
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM zap_metadata WHERE sender_pubkey = $1",
        )
        .bind(pubkey)
        .fetch_one(&self.pool)
        .await?;

        let order_clause = match sort {
            "amount" => "ORDER BY zm.amount_msats DESC",
            _ => "ORDER BY zm.created_at DESC", // "recent" or default
        };

        let query = format!(
            r#"
            SELECT
                e.id, e.pubkey, e.created_at, e.kind, e.content, e.sig, e.tags, e.raw, e.relay_url, e.received_at,
                zm.amount_msats,
                zm.recipient_pubkey,
                zm.zapped_event_id
            FROM zap_metadata zm
            JOIN events e ON e.id = zm.event_id
            WHERE zm.sender_pubkey = $1
            {}
            LIMIT $2 OFFSET $3
            "#,
            order_clause
        );

        let rows = sqlx::query(&query)
        .bind(pubkey)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        let mut entries = Vec::with_capacity(rows.len());
        let mut counterparty_pubkeys = HashSet::new();
        counterparty_pubkeys.insert(pubkey.to_string());

        for row in &rows {
            let event = StoredEvent {
                id: row.try_get("id")?,
                pubkey: row.try_get("pubkey")?,
                created_at: row.try_get("created_at")?,
                kind: row.try_get("kind")?,
                content: row.try_get("content")?,
                sig: row.try_get("sig")?,
                tags: row.try_get("tags")?,
                raw: row.try_get("raw")?,
                relay_url: row.try_get("relay_url")?,
                received_at: row.try_get("received_at")?,
            };
            let amount_msats: i64 = row.try_get("amount_msats")?;
            let recipient: Option<String> = row.try_get("recipient_pubkey")?;
            let zapped_event_id: Option<String> = row.try_get("zapped_event_id")?;

            if let Some(ref r) = recipient {
                counterparty_pubkeys.insert(r.clone());
            }

            entries.push(super::models::ProfileZapEntry {
                event,
                amount_sats: amount_msats / 1000,
                counterparty: recipient,
                zapped_event_id,
            });
        }

        let profile_rows = self
            .latest_profile_metadata(&counterparty_pubkeys.into_iter().collect::<Vec<_>>())
            .await?;

        Ok((entries, total, profile_rows))
    }

    /// Zaps received by a pubkey (recipient is the `p` tag).
    pub async fn profile_zaps_received(
        &self,
        pubkey: &str,
        limit: i64,
        offset: i64,
        sort: &str,
    ) -> Result<(Vec<super::models::ProfileZapEntry>, i64, Vec<ProfileRow>), AppError> {
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM zap_metadata WHERE recipient_pubkey = $1",
        )
        .bind(pubkey)
        .fetch_one(&self.pool)
        .await?;

        let order_clause = match sort {
            "amount" => "ORDER BY zm.amount_msats DESC",
            _ => "ORDER BY zm.created_at DESC", // "recent" or default
        };

        let query = format!(
            r#"
            SELECT
                e.id, e.pubkey, e.created_at, e.kind, e.content, e.sig, e.tags, e.raw, e.relay_url, e.received_at,
                zm.amount_msats,
                zm.sender_pubkey,
                zm.zapped_event_id
            FROM zap_metadata zm
            JOIN events e ON e.id = zm.event_id
            WHERE zm.recipient_pubkey = $1
            {}
            LIMIT $2 OFFSET $3
            "#,
            order_clause
        );

        let rows = sqlx::query(&query)
        .bind(pubkey)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        let mut entries = Vec::with_capacity(rows.len());
        let mut counterparty_pubkeys = HashSet::new();
        counterparty_pubkeys.insert(pubkey.to_string());

        for row in &rows {
            let event = StoredEvent {
                id: row.try_get("id")?,
                pubkey: row.try_get("pubkey")?,
                created_at: row.try_get("created_at")?,
                kind: row.try_get("kind")?,
                content: row.try_get("content")?,
                sig: row.try_get("sig")?,
                tags: row.try_get("tags")?,
                raw: row.try_get("raw")?,
                relay_url: row.try_get("relay_url")?,
                received_at: row.try_get("received_at")?,
            };
            let amount_msats: i64 = row.try_get("amount_msats")?;
            let sender: Option<String> = row.try_get("sender_pubkey")?;
            let zapped_event_id: Option<String> = row.try_get("zapped_event_id")?;

            if let Some(ref s) = sender {
                counterparty_pubkeys.insert(s.clone());
            }

            entries.push(super::models::ProfileZapEntry {
                event,
                amount_sats: amount_msats / 1000,
                counterparty: sender,
                zapped_event_id,
            });
        }

        let profile_rows = self
            .latest_profile_metadata(&counterparty_pubkeys.into_iter().collect::<Vec<_>>())
            .await?;

        Ok((entries, total, profile_rows))
    }

    /// Return ALL follower pubkeys for a given pubkey (no limit).
    pub async fn all_follower_pubkeys(
        &self,
        pubkey: &str,
    ) -> Result<Vec<String>, AppError> {
        let rows = sqlx::query_scalar::<_, String>(
            "SELECT follower_pubkey
             FROM follows
             WHERE followed_pubkey = $1
             ORDER BY created_at DESC",
        )
        .bind(pubkey)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    /// A pubkey's kind-1 notes ranked by a specific metric, filtered by root/reply status.
    ///
    /// `pubkey`: hex pubkey to scope the query
    /// `is_reply`: false = root notes only, true = replies only
    /// `metric`: "likes" | "reposts" | "zaps" | "replies"
    pub async fn ranked_notes_by_pubkey(
        &self,
        pubkey: &str,
        is_reply: bool,
        metric: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<RankedEvent>, Vec<ProfileRow>), AppError> {
        let order_col = match metric {
            "likes" => "reaction_count",
            "reposts" => "repost_count",
            "zaps" => "zap_amount_msats",
            "replies" => "reply_count",
            _ => "reaction_count",
        };

        let reply_filter = if is_reply { "AND e.is_reply = true" } else { "AND NOT e.is_reply" };

        let sql = format!(
            r#"
            SELECT
                e.id, e.pubkey, e.created_at, e.kind, e.content, e.sig,
                e.tags, e.raw, e.relay_url, e.received_at,
                e.reaction_count::bigint AS reactions,
                e.repost_count::bigint AS reposts,
                e.reply_count::bigint AS replies,
                e.zap_count::bigint AS zaps,
                e.zap_amount_msats,
                {order_col}::bigint AS metric_count
            FROM events e
            WHERE e.kind = 1
              AND e.pubkey = $1
              {reply_filter}
              AND {order_col} > 0
            ORDER BY {order_col} DESC, e.created_at DESC
            LIMIT $2 OFFSET $3
            "#
        );

        let rows = sqlx::query(&sql)
            .bind(pubkey)
            .bind(limit)
            .bind(offset)
            .fetch_all(&self.pool)
            .await?;

        let is_zap = metric == "zaps";

        let mut pubkeys = Vec::new();
        let events: Vec<RankedEvent> = rows
            .into_iter()
            .map(|row| -> Result<RankedEvent, sqlx::Error> {
                let pubkey: String = row.try_get("pubkey")?;
                pubkeys.push(pubkey.clone());
                let event = StoredEvent {
                    id: row.try_get("id")?,
                    pubkey,
                    created_at: row.try_get("created_at")?,
                    kind: row.try_get("kind")?,
                    content: row.try_get("content")?,
                    sig: row.try_get("sig")?,
                    tags: row.try_get("tags")?,
                    raw: row.try_get("raw")?,
                    relay_url: row.try_get("relay_url").ok(),
                    received_at: row.try_get("received_at")?,
                };
                let count: i64 = row.try_get("metric_count")?;
                let zap_msats: i64 = row.try_get("zap_amount_msats")?;
                let total_sats = if is_zap { Some(zap_msats / 1000) } else { None };
                Ok(RankedEvent {
                    event,
                    count,
                    total_sats,
                    reactions: row.try_get::<i64, _>("reactions").unwrap_or(0),
                    replies: row.try_get::<i64, _>("replies").unwrap_or(0),
                    reposts: row.try_get::<i64, _>("reposts").unwrap_or(0),
                    zap_sats: zap_msats / 1000,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        let unique_pubkeys: Vec<String> = {
            let mut seen = HashSet::new();
            pubkeys
                .into_iter()
                .filter(|pk| seen.insert(pk.clone()))
                .collect()
        };
        let profiles = self.latest_profile_metadata(&unique_pubkeys).await?;

        Ok((events, profiles))
    }

    /// Aggregate zap stats for a pubkey (total sent + received).
    pub async fn profile_zap_stats(
        &self,
        pubkey: &str,
    ) -> Result<super::models::ProfileZapStats, AppError> {
        let sent_row: (i64, i64) = sqlx::query_as(
            "SELECT COALESCE(SUM(amount_msats), 0)::bigint / 1000, COUNT(*)::bigint FROM zap_metadata WHERE sender_pubkey = $1",
        )
        .bind(pubkey)
        .fetch_one(&self.pool)
        .await?;

        let recv_row: (i64, i64) = sqlx::query_as(
            "SELECT COALESCE(SUM(amount_msats), 0)::bigint / 1000, COUNT(*)::bigint FROM zap_metadata WHERE recipient_pubkey = $1",
        )
        .bind(pubkey)
        .fetch_one(&self.pool)
        .await?;

        Ok(super::models::ProfileZapStats {
            pubkey: pubkey.to_string(),
            sent: super::models::ZapAggregate {
                total_sats: sent_row.0,
                zap_count: sent_row.1,
            },
            received: super::models::ZapAggregate {
                total_sats: recv_row.0,
                zap_count: recv_row.1,
            },
        })
    }

    // -----------------------------------------------------------------------
    // Missing event queue
    // -----------------------------------------------------------------------

    /// Queue an event ID for on-demand fetching.
    /// Called when a counter update (reaction/repost/zap) hits a target we don't have.
    /// Idempotent — raises priority if a higher-priority entry arrives later.
    pub async fn queue_missing_event(
        &self,
        event_id: &str,
        relay_hint: Option<&str>,
        priority: i16,
    ) -> Result<(), AppError> {
        if event_id.len() != 64 || !event_id.chars().all(|c| c.is_ascii_hexdigit()) {
            return Ok(());
        }
        sqlx::query(
            "INSERT INTO missing_events (event_id, relay_hint, priority)
             VALUES ($1, $2, $3)
             ON CONFLICT (event_id) DO UPDATE SET
                 priority  = GREATEST(missing_events.priority, EXCLUDED.priority),
                 relay_hint = COALESCE(EXCLUDED.relay_hint, missing_events.relay_hint)
             WHERE NOT missing_events.fetched",
        )
        .bind(event_id)
        .bind(relay_hint)
        .bind(priority)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Pull up to `limit` unfetched missing events, ordered by priority then discovery time.
    pub async fn take_missing_events(&self, limit: i64) -> Result<Vec<MissingEvent>, AppError> {
        let rows = sqlx::query_as::<_, MissingEvent>(
            "SELECT event_id, relay_hint, priority
             FROM missing_events
             WHERE fetched = false AND attempt_count < 5
             ORDER BY priority DESC, discovered_at ASC
             LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Mark a missing event as successfully fetched.
    pub async fn mark_missing_event_fetched(&self, event_id: &str) -> Result<(), AppError> {
        sqlx::query("UPDATE missing_events SET fetched = true WHERE event_id = $1")
            .bind(event_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Record a failed fetch attempt so we back off and eventually give up.
    pub async fn mark_missing_event_attempted(&self, event_id: &str) -> Result<(), AppError> {
        sqlx::query(
            "UPDATE missing_events
             SET attempt_count = attempt_count + 1, last_attempted_at = NOW()
             WHERE event_id = $1",
        )
        .bind(event_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Reapply engagement counters for an event that was just fetched from relays.
    /// Uses stored seen_events (reactions/reposts) and zap_metadata (zaps) as the
    /// source of truth, since the event had zero counters when first inserted.
    pub async fn reapply_counters_for_event(&self, event_id: &str) -> Result<(), AppError> {
        sqlx::query(
            "UPDATE events SET
                reaction_count    = (SELECT COUNT(*) FROM seen_events
                                     WHERE target_id = $1 AND kind = 7),
                repost_count      = (SELECT COUNT(*) FROM seen_events
                                     WHERE target_id = $1 AND kind IN (6, 16)),
                zap_count         = (SELECT COUNT(*) FROM zap_metadata
                                     WHERE zapped_event_id = $1),
                zap_amount_msats  = (SELECT COALESCE(SUM(amount_msats), 0) FROM zap_metadata
                                     WHERE zapped_event_id = $1)
             WHERE id = $1",
        )
        .bind(event_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

enum BindValue {
    Text(String),
}

fn is_hex_pubkey(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|c| c.is_ascii_hexdigit())
}
