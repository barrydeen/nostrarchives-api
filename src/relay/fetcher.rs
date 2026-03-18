use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{Mutex, Notify};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use crate::cache::StatsCache;
use crate::crawler::relay_router::RelayRouter;
use crate::db::models::{NostrEvent, StoredEvent};
use crate::db::repository::EventRepository;
use crate::error::AppError;

const NEGATIVE_CACHE_TTL: u64 = 300; // 5 minutes

/// On-demand relay fetcher service for pulling specific data from relays
/// when it's not available in the local database.
pub struct RelayFetcher {
    repo: EventRepository,
    relay_router: RelayRouter,
    cache: StatsCache,
    default_relays: Vec<String>,
    timeout_ms: u64,
    max_relays: usize,
    enabled: bool,
    /// Inflight request coalescing to avoid duplicate fetches
    inflight: Arc<Mutex<HashMap<String, Arc<Notify>>>>,
}

impl RelayFetcher {
    pub fn new(
        repo: EventRepository,
        relay_router: RelayRouter,
        default_relays: Vec<String>,
        cache: StatsCache,
        timeout_ms: u64,
        max_relays: usize,
        enabled: bool,
    ) -> Self {
        Self {
            repo,
            relay_router,
            cache,
            default_relays,
            timeout_ms,
            max_relays,
            enabled,
            inflight: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Fetch a single event by its hex ID from relays, store it, and return it.
    pub async fn fetch_event_by_id(
        &self,
        id: &str,
        relay_hints: &[String],
    ) -> Result<Option<StoredEvent>, AppError> {
        if !self.enabled {
            return Ok(None);
        }
        let cache_key = format!("fetch:miss:event:{}", id);

        // Check negative cache first
        if self.cache.get_json(&cache_key).await.is_some() {
            return Ok(None);
        }

        // Request coalescing
        let notify = {
            let mut inflight = self.inflight.lock().await;
            if let Some(existing) = inflight.get(id) {
                let notify = existing.clone();
                drop(inflight);
                notify.notified().await;
                // Check DB again after the other request completed
                return Ok(self.repo.get_event_by_id(id).await?);
            } else {
                let notify = Arc::new(Notify::new());
                inflight.insert(id.to_string(), notify.clone());
                notify
            }
        };

        let result = self.fetch_event_by_id_internal(id, relay_hints).await;

        // Clean up and notify waiters
        {
            let mut inflight = self.inflight.lock().await;
            inflight.remove(id);
        }
        notify.notify_waiters();

        match result {
            Ok(Some(event)) => Ok(Some(event)),
            Ok(None) => {
                // Cache negative result
                self.cache
                    .set_json(&cache_key, "not_found", NEGATIVE_CACHE_TTL)
                    .await;
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    async fn fetch_event_by_id_internal(
        &self,
        id: &str,
        relay_hints: &[String],
    ) -> Result<Option<StoredEvent>, AppError> {
        let filter = serde_json::json!({
            "ids": [id],
            "limit": 1
        });

        let relays = self.select_relays_for_fetch(relay_hints, &[]).await;
        let events = self.fetch_from_relays(&relays, filter).await?;

        for event in events {
            if event.id == id {
                // Store the event
                if let Err(e) = self.repo.insert_event(&event, "on-demand").await {
                    tracing::warn!(
                        event_id = %event.id,
                        error = %e,
                        "failed to store fetched event"
                    );
                }
                // Return it as StoredEvent
                return Ok(self.repo.get_event_by_id(id).await?);
            }
        }

        Ok(None)
    }

    /// Fetch kind-0 metadata for a pubkey from relays.
    pub async fn fetch_profile_metadata(
        &self,
        pubkey: &str,
        relay_hints: &[String],
    ) -> Result<Option<Value>, AppError> {
        if !self.enabled {
            return Ok(None);
        }
        let cache_key = format!("fetch:miss:profile:{}", pubkey);

        // Check negative cache first
        if self.cache.get_json(&cache_key).await.is_some() {
            return Ok(None);
        }

        // Request coalescing
        let notify = {
            let mut inflight = self.inflight.lock().await;
            let key = format!("profile:{}", pubkey);
            if let Some(existing) = inflight.get(&key) {
                let notify = existing.clone();
                drop(inflight);
                notify.notified().await;
                // Check DB again after the other request completed
                return self.get_profile_metadata_from_db(pubkey).await;
            } else {
                let notify = Arc::new(Notify::new());
                inflight.insert(key.clone(), notify.clone());
                notify
            }
        };

        let result = self.fetch_profile_metadata_internal(pubkey, relay_hints).await;

        // Clean up and notify waiters
        {
            let mut inflight = self.inflight.lock().await;
            inflight.remove(&format!("profile:{}", pubkey));
        }
        notify.notify_waiters();

        match result {
            Ok(Some(metadata)) => Ok(Some(metadata)),
            Ok(None) => {
                // Cache negative result
                self.cache
                    .set_json(&cache_key, "not_found", NEGATIVE_CACHE_TTL)
                    .await;
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    async fn fetch_profile_metadata_internal(
        &self,
        pubkey: &str,
        relay_hints: &[String],
    ) -> Result<Option<Value>, AppError> {
        let filter = serde_json::json!({
            "authors": [pubkey],
            "kinds": [0],
            "limit": 1
        });

        let relays = self.select_relays_for_fetch(relay_hints, &[pubkey.to_string()]).await;
        let events = self.fetch_from_relays(&relays, filter).await?;

        for event in events {
            if event.pubkey == pubkey && event.kind == 0 {
                // Store the event
                if let Err(e) = self.repo.insert_event(&event, "on-demand").await {
                    tracing::warn!(
                        pubkey = %event.pubkey,
                        error = %e,
                        "failed to store fetched profile metadata"
                    );
                }
                // Parse and return the content
                if let Ok(content) = serde_json::from_str::<Value>(&event.content) {
                    return Ok(Some(content));
                }
            }
        }

        Ok(None)
    }

    /// Fetch recent content for an author: kind-1 notes + kind-0 metadata + 
    /// kind-3 contact list + kind-10002 relay list. Returns count of new events stored.
    pub async fn fetch_author_content(
        &self,
        pubkey: &str,
        relay_hints: &[String],
    ) -> Result<i64, AppError> {
        if !self.enabled {
            return Ok(0);
        }
        let cache_key = format!("fetch:miss:author:{}", pubkey);

        // Check negative cache first
        if self.cache.get_json(&cache_key).await.is_some() {
            return Ok(0);
        }

        // Request coalescing
        let notify = {
            let mut inflight = self.inflight.lock().await;
            let key = format!("author:{}", pubkey);
            if let Some(existing) = inflight.get(&key) {
                let notify = existing.clone();
                drop(inflight);
                notify.notified().await;
                return Ok(0); // Other request handled it
            } else {
                let notify = Arc::new(Notify::new());
                inflight.insert(key.clone(), notify.clone());
                notify
            }
        };

        let result = self.fetch_author_content_internal(pubkey, relay_hints).await;

        // Clean up and notify waiters
        {
            let mut inflight = self.inflight.lock().await;
            inflight.remove(&format!("author:{}", pubkey));
        }
        notify.notify_waiters();

        match result {
            Ok(count) if count > 0 => Ok(count),
            Ok(_) => {
                // Cache negative result if nothing was found
                self.cache
                    .set_json(&cache_key, "not_found", NEGATIVE_CACHE_TTL)
                    .await;
                Ok(0)
            }
            Err(e) => Err(e),
        }
    }

    async fn fetch_author_content_internal(
        &self,
        pubkey: &str,
        relay_hints: &[String],
    ) -> Result<i64, AppError> {
        let filter = serde_json::json!({
            "authors": [pubkey],
            "kinds": [0, 1, 3, 10002],
            "limit": 100
        });

        let relays = self.select_relays_for_fetch(relay_hints, &[pubkey.to_string()]).await;
        let events = self.fetch_from_relays(&relays, filter).await?;

        let mut stored_count = 0i64;
        for event in events {
            if event.pubkey == pubkey {
                match self.repo.insert_event(&event, "on-demand").await {
                    Ok(true) => stored_count += 1,
                    Ok(false) => {} // duplicate, already stored
                    Err(e) => {
                        tracing::warn!(
                            pubkey = %event.pubkey,
                            kind = event.kind,
                            error = %e,
                            "failed to store fetched author event"
                        );
                    }
                }
            }
        }

        tracing::info!(
            pubkey = %pubkey,
            stored = stored_count,
            "fetched author content"
        );

        Ok(stored_count)
    }

    /// Check which pubkeys are missing metadata and batch-fetch them.
    pub async fn ensure_profiles(&self, pubkeys: &[String]) -> Result<(), AppError> {
        if !self.enabled || pubkeys.is_empty() {
            return Ok(());
        }

        // Check which ones need metadata
        let missing = self.repo.pubkeys_missing_metadata(pubkeys).await?;
        if missing.is_empty() {
            return Ok(());
        }

        tracing::info!(
            requested = pubkeys.len(),
            missing = missing.len(),
            "ensuring profiles"
        );

        // Fetch metadata for missing ones
        for chunk in missing.chunks(50) {
            self.fetch_profiles_batch(chunk).await?;
        }

        Ok(())
    }

    async fn fetch_profiles_batch(&self, pubkeys: &[String]) -> Result<(), AppError> {
        let filter = serde_json::json!({
            "authors": pubkeys,
            "kinds": [0],
            "limit": pubkeys.len()
        });

        let relays = self.select_relays_for_fetch(&[], pubkeys).await;
        let events = self.fetch_from_relays(&relays, filter).await?;

        let mut stored_count = 0;
        for event in events {
            if event.kind == 0 && pubkeys.contains(&event.pubkey) {
                match self.repo.insert_event(&event, "on-demand").await {
                    Ok(true) => stored_count += 1,
                    Ok(false) => {} // duplicate
                    Err(e) => {
                        tracing::warn!(
                            pubkey = %event.pubkey,
                            error = %e,
                            "failed to store fetched profile"
                        );
                    }
                }
            }
        }

        tracing::info!(
            requested = pubkeys.len(),
            stored = stored_count,
            "batch profile fetch completed"
        );

        Ok(())
    }

    /// Select relays to use for fetching, prioritizing hints, then relay lists, then defaults.
    async fn select_relays_for_fetch(
        &self,
        relay_hints: &[String],
        pubkeys: &[String],
    ) -> Vec<String> {
        let mut relays = Vec::new();

        // 1. Add relay hints
        for hint in relay_hints {
            if !hint.is_empty() {
                relays.push(hint.clone());
            }
        }

        // 2. Add relays from NIP-65 relay lists
        if !pubkeys.is_empty() && relays.len() < self.max_relays {
            if let Ok(relay_groups) = self.relay_router.get_relay_author_groups(pubkeys).await {
                for (relay_url, _authors) in relay_groups {
                    if !relays.contains(&relay_url) {
                        relays.push(relay_url);
                        if relays.len() >= self.max_relays {
                            break;
                        }
                    }
                }
            }
        }

        // 3. Add fallback relays
        if relays.len() < self.max_relays {
            for default in &self.default_relays {
                if !relays.contains(default) {
                    relays.push(default.clone());
                    if relays.len() >= self.max_relays {
                        break;
                    }
                }
            }
        }

        // Limit to max relays
        relays.truncate(self.max_relays);
        relays
    }

    /// Fetch events from multiple relays with the given filter.
    /// Stops early once results are found (no need to query all relays).
    async fn fetch_from_relays(
        &self,
        relays: &[String],
        filter: Value,
    ) -> Result<Vec<NostrEvent>, AppError> {
        let timeout_duration = Duration::from_millis(self.timeout_ms);

        for relay_url in relays {
            match self.fetch_from_relay(relay_url, &filter, timeout_duration).await {
                Ok(events) if !events.is_empty() => {
                    return Ok(events);
                }
                Ok(_) => {} // empty, try next relay
                Err(e) => {
                    tracing::warn!(
                        relay = relay_url,
                        error = %e,
                        "relay fetch failed"
                    );
                }
            }
        }

        Ok(vec![])
    }

    /// Connect to a single relay and fetch events matching the filter.
    async fn fetch_from_relay(
        &self,
        relay_url: &str,
        filter: &Value,
        timeout_duration: Duration,
    ) -> Result<Vec<NostrEvent>, String> {
        let (ws_stream, _) = tokio_tungstenite::connect_async(relay_url)
            .await
            .map_err(|e| format!("connect failed: {e}"))?;
        let (mut write, mut read) = ws_stream.split();

        let sub_id = format!("fetch-{}", Uuid::new_v4().simple());
        let req = serde_json::json!(["REQ", &sub_id, filter]);

        write
            .send(Message::Text(req.to_string().into()))
            .await
            .map_err(|e| format!("send REQ failed: {e}"))?;

        let mut events = Vec::new();

        loop {
            match timeout(timeout_duration, read.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    let parsed: Value = match serde_json::from_str(&text) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    let arr = match parsed.as_array() {
                        Some(a) if a.len() >= 3 => a,
                        _ => continue,
                    };
                    let msg_type = match arr[0].as_str() {
                        Some(t) => t,
                        None => continue,
                    };

                    if msg_type == "EOSE" {
                        break; // All events received
                    }

                    if msg_type != "EVENT" {
                        continue;
                    }

                    if let Ok(event) = serde_json::from_value::<NostrEvent>(arr[2].clone()) {
                        events.push(event);
                    }
                }
                Ok(Some(Ok(Message::Ping(data)))) => {
                    let _ = write.send(Message::Pong(data)).await;
                }
                Ok(Some(Ok(Message::Close(_)))) | Ok(None) => break,
                Ok(Some(Err(e))) => return Err(format!("ws error: {e}")),
                Err(_) => break, // timeout
                _ => {}
            }
        }

        // Send CLOSE
        let close_msg = serde_json::json!(["CLOSE", &sub_id]);
        let _ = write
            .send(Message::Text(close_msg.to_string().into()))
            .await;

        Ok(events)
    }

    /// Helper to get profile metadata from DB.
    async fn get_profile_metadata_from_db(&self, pubkey: &str) -> Result<Option<Value>, AppError> {
        // Check if we have kind-0 metadata for this pubkey in the DB
        let profile_rows = self.repo.latest_profile_metadata(&[pubkey.to_string()]).await?;
        
        for profile in profile_rows {
            if profile.pubkey == pubkey {
                if let Ok(content) = serde_json::from_str::<Value>(&profile.content) {
                    return Ok(Some(content));
                }
            }
        }
        
        Ok(None)
    }
}
