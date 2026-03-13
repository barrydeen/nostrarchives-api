use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::broadcast;
use tokio::time::{interval, sleep, timeout};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use super::queue::{CrawlQueue, CrawlTarget};
use crate::cache::StatsCache;
use crate::db::models::NostrEvent;
use crate::db::repository::EventRepository;

/// Configuration for the crawler worker.
#[derive(Debug, Clone)]
pub struct CrawlerConfig {
    /// Relay URLs to crawl from (use indexer relays for best coverage).
    pub relay_urls: Vec<String>,
    /// How many authors to crawl per batch.
    pub batch_size: i64,
    /// Max events to request per author per relay query.
    pub events_per_author: i64,
    /// Delay between relay requests to avoid rate limiting (ms).
    pub request_delay_ms: u64,
    /// How often to check for new work (seconds).
    pub poll_interval_secs: u64,
    /// How often to re-sync the queue from follows table (seconds).
    pub sync_interval_secs: u64,
    /// Max concurrent relay connections for crawling.
    pub max_concurrency: usize,
}

impl Default for CrawlerConfig {
    fn default() -> Self {
        Self {
            relay_urls: vec![],
            batch_size: 10,
            events_per_author: 500,
            request_delay_ms: 500,
            poll_interval_secs: 30,
            sync_interval_secs: 3600,
            max_concurrency: 3,
        }
    }
}

/// The intelligent crawler: fetches historical notes for known authors,
/// prioritized by follower count, recent content first.
pub struct Crawler {
    config: CrawlerConfig,
    queue: CrawlQueue,
    repo: EventRepository,
    cache: StatsCache,
    total_crawled: Arc<AtomicU64>,
}

impl Crawler {
    pub fn new(
        config: CrawlerConfig,
        queue: CrawlQueue,
        repo: EventRepository,
        cache: StatsCache,
    ) -> Self {
        Self {
            config,
            queue,
            repo,
            cache,
            total_crawled: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Start the crawler. Runs until shutdown signal.
    pub async fn run(self, shutdown: broadcast::Sender<()>) {
        let queue = self.queue.clone();
        let config = self.config.clone();

        // Initial sync: seed queue from follows table
        match queue.sync_from_follows().await {
            Ok(stats) => {
                tracing::info!(
                    new = stats.new_pubkeys,
                    updated = stats.updated_counts,
                    "crawler queue seeded from follows"
                );
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to seed crawler queue");
            }
        }

        // Sync cursors so we know what we already have
        match queue.sync_cursors_from_events().await {
            Ok(n) => {
                tracing::info!(updated = n, "synced crawl cursors from existing events");
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to sync crawl cursors");
            }
        }

        // Log initial queue state
        if let Ok(stats) = queue.stats().await {
            tracing::info!(
                total = stats.total_authors,
                tier1 = stats.tier1_authors,
                tier2 = stats.tier2_authors,
                tier3 = stats.tier3_authors,
                tier4 = stats.tier4_authors,
                ready = stats.ready_to_crawl,
                "crawler queue initialized"
            );
        }

        // Spawn periodic queue sync (re-rank authors as new follows come in)
        let sync_queue = queue.clone();
        let sync_interval = config.sync_interval_secs;
        let mut sync_shutdown = shutdown.subscribe();
        tokio::spawn(async move {
            let mut tick = interval(Duration::from_secs(sync_interval));
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        match sync_queue.sync_from_follows().await {
                            Ok(stats) => {
                                if stats.new_pubkeys > 0 || stats.updated_counts > 0 {
                                    tracing::info!(
                                        new = stats.new_pubkeys,
                                        updated = stats.updated_counts,
                                        "crawler queue re-synced"
                                    );
                                }
                            }
                            Err(e) => tracing::warn!(error = %e, "queue sync failed"),
                        }
                    }
                    _ = sync_shutdown.recv() => return,
                }
            }
        });

        // Main crawl loop
        let mut poll_tick = interval(Duration::from_secs(config.poll_interval_secs));
        let mut shutdown_rx = shutdown.subscribe();

        loop {
            tokio::select! {
                _ = poll_tick.tick() => {
                    let batch = match self.queue.take_batch(self.config.batch_size).await {
                        Ok(b) => b,
                        Err(e) => {
                            tracing::warn!(error = %e, "failed to take crawl batch");
                            continue;
                        }
                    };

                    if batch.is_empty() {
                        continue;
                    }

                    tracing::info!(
                        batch_size = batch.len(),
                        tiers = ?batch.iter().map(|t| t.priority_tier).collect::<Vec<_>>(),
                        "crawling batch"
                    );

                    for target in batch {
                        if shutdown.receiver_count() == 0 {
                            return;
                        }
                        self.crawl_author(&target).await;

                        // Delay between authors to be relay-friendly
                        if self.config.request_delay_ms > 0 {
                            sleep(Duration::from_millis(self.config.request_delay_ms)).await;
                        }
                    }
                }
                _ = shutdown_rx.recv() => {
                    tracing::info!("crawler shutting down");
                    return;
                }
            }
        }
    }

    /// Crawl a single author's notes from relays, then backfill engagement.
    ///
    /// Strategy:
    /// 1. If we have newest_seen_at, fetch notes AFTER that (new content since last crawl).
    /// 2. If we have crawl_cursor, also fetch notes BEFORE that (historical backfill).
    /// 3. If neither, start with recent notes (no since filter, relay returns newest).
    /// 4. Fetch engagement (reactions, reposts, zaps) referencing this author's notes.
    async fn crawl_author(&self, target: &CrawlTarget) {
        let mut total_new = 0i64;
        let mut oldest_ts: Option<i64> = None;
        let mut newest_ts: Option<i64> = None;

        // Phase 1: Fetch new notes (since newest_seen_at)
        if let Some(newest) = target.newest_seen_at {
            let since = newest + 1; // Don't re-fetch the exact timestamp
            let result = self
                .fetch_author_notes(&target.pubkey, Some(since), None)
                .await;
            if let Some((new, oldest, newest_found)) = result {
                total_new += new;
                oldest_ts = merge_min(oldest_ts, oldest);
                newest_ts = merge_max(newest_ts, newest_found);
            }
        }

        // Phase 2: Historical backfill (before crawl_cursor) OR initial crawl
        if target.crawl_cursor.is_some() || target.newest_seen_at.is_none() {
            let until = target.crawl_cursor; // Fetch older than what we've already gotten
            let result = self.fetch_author_notes(&target.pubkey, None, until).await;
            if let Some((new, oldest, newest_found)) = result {
                total_new += new;
                oldest_ts = merge_min(oldest_ts, oldest);
                newest_ts = merge_max(newest_ts, newest_found);
            }
        }

        // Phase 3: Backfill engagement (replies/reactions/reposts/zaps) for this author's notes.
        // Tier 1-3 authors (10+ followers) to capture reply threads.
        let mut engagement_new = 0i64;
        if target.priority_tier <= 3 {
            engagement_new = self.crawl_engagement(&target.pubkey).await;
        }

        // Update crawl state
        let combined_new = total_new + engagement_new;
        if let Err(e) = self
            .queue
            .mark_crawled(&target.pubkey, total_new, oldest_ts, newest_ts)
            .await
        {
            tracing::warn!(
                pubkey = %target.pubkey,
                error = %e,
                "failed to update crawl state"
            );
        }

        if combined_new > 0 {
            let global = self
                .total_crawled
                .fetch_add(total_new as u64, Ordering::Relaxed)
                + total_new as u64;
            tracing::info!(
                pubkey = %&target.pubkey[..12],
                followers = target.follower_count,
                tier = target.priority_tier,
                new_notes = total_new,
                engagement = engagement_new,
                total_crawled = global,
                "author crawled"
            );
        }
    }

    /// Fetch engagement events (kinds 6, 7, 9735) that reference this author's notes.
    /// Uses the `#p` tag filter to find reactions/reposts/zaps targeting this pubkey.
    async fn crawl_engagement(&self, pubkey: &str) -> i64 {
        let max_relays = self
            .config
            .max_concurrency
            .min(self.config.relay_urls.len());
        let mut total_new = 0i64;

        for relay_url in self.config.relay_urls.iter().take(max_relays) {
            match self.fetch_engagement_from_relay(relay_url, pubkey).await {
                Ok(events) => {
                    for event in &events {
                        match self.repo.insert_event(event, relay_url).await {
                            Ok(true) => {
                                total_new += 1;
                                self.cache
                                    .on_event_ingested(&event.pubkey, event.kind)
                                    .await;
                            }
                            Ok(false) => {} // duplicate
                            Err(e) => {
                                tracing::warn!(
                                    event_id = %event.id,
                                    error = %e,
                                    "crawler: failed to insert engagement event"
                                );
                            }
                        }
                    }
                    if !events.is_empty() {
                        break;
                    }
                }
                Err(e) => {
                    tracing::debug!(
                        relay = relay_url,
                        pubkey = %&pubkey[..12],
                        error = %e,
                        "crawler: engagement fetch failed"
                    );
                }
            }

            if self.config.request_delay_ms > 0 {
                sleep(Duration::from_millis(self.config.request_delay_ms)).await;
            }
        }

        total_new
    }

    /// Fetch engagement events from a single relay for a given author (via #p tag).
    async fn fetch_engagement_from_relay(
        &self,
        relay_url: &str,
        pubkey: &str,
    ) -> Result<Vec<NostrEvent>, String> {
        let connect_timeout = Duration::from_secs(10);
        let msg_timeout = Duration::from_secs(15);

        let (ws_stream, _) = timeout(connect_timeout, tokio_tungstenite::connect_async(relay_url))
            .await
            .map_err(|_| "connect timeout".to_string())?
            .map_err(|e| format!("connect failed: {e}"))?;

        let (mut write, mut read) = ws_stream.split();

        // Fetch replies, reactions, reposts, and zaps that tag this pubkey
        let filter = serde_json::json!({
            "kinds": [1, 6, 7, 9735],
            "#p": [pubkey],
            "limit": self.config.events_per_author,
        });

        let sub_id = format!("eng-{}", Uuid::new_v4().simple());
        let req = serde_json::json!(["REQ", &sub_id, filter]);

        write
            .send(Message::Text(req.to_string().into()))
            .await
            .map_err(|e| format!("send REQ failed: {e}"))?;

        let mut events = Vec::new();
        let allowed = [1i64, 6, 7, 9735];

        loop {
            match timeout(msg_timeout, read.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    let parsed: Value = match serde_json::from_str(&text) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    let arr = match parsed.as_array() {
                        Some(a) if a.len() >= 2 => a,
                        _ => continue,
                    };

                    match arr[0].as_str() {
                        Some("EVENT") if arr.len() >= 3 => {
                            if let Ok(event) = serde_json::from_value::<NostrEvent>(arr[2].clone())
                            {
                                if allowed.contains(&event.kind) {
                                    events.push(event);
                                }
                            }
                        }
                        Some("EOSE") => break,
                        Some("CLOSED") | Some("NOTICE") => break,
                        _ => {}
                    }
                }
                Ok(Some(Ok(Message::Ping(data)))) => {
                    let _ = write.send(Message::Pong(data)).await;
                }
                Ok(Some(Ok(Message::Close(_)))) | Ok(None) => break,
                Ok(Some(Err(e))) => return Err(format!("ws error: {e}")),
                Err(_) => break,
                _ => {}
            }
        }

        let close_msg = serde_json::json!(["CLOSE", &sub_id]);
        let _ = write
            .send(Message::Text(close_msg.to_string().into()))
            .await;

        Ok(events)
    }

    /// Fetch kind-1 notes for a specific author from relays.
    /// Returns (new_events_inserted, oldest_timestamp, newest_timestamp) or None on total failure.
    async fn fetch_author_notes(
        &self,
        pubkey: &str,
        since: Option<i64>,
        until: Option<i64>,
    ) -> Option<(i64, Option<i64>, Option<i64>)> {
        let mut total_new = 0i64;
        let mut oldest: Option<i64> = None;
        let mut newest: Option<i64> = None;

        // Try relays until we get results or exhaust the list
        let max_relays = self
            .config
            .max_concurrency
            .min(self.config.relay_urls.len());

        for relay_url in self.config.relay_urls.iter().take(max_relays) {
            match self.fetch_from_relay(relay_url, pubkey, since, until).await {
                Ok(events) => {
                    for event in &events {
                        oldest = merge_min(oldest, Some(event.created_at));
                        newest = merge_max(newest, Some(event.created_at));

                        match self.repo.insert_event(event, relay_url).await {
                            Ok(true) => {
                                total_new += 1;
                                self.cache
                                    .on_event_ingested(&event.pubkey, event.kind)
                                    .await;
                            }
                            Ok(false) => {} // duplicate
                            Err(e) => {
                                tracing::warn!(
                                    event_id = %event.id,
                                    error = %e,
                                    "crawler: failed to insert event"
                                );
                            }
                        }
                    }

                    // If we got results from this relay, don't query more
                    if !events.is_empty() {
                        break;
                    }
                }
                Err(e) => {
                    tracing::debug!(
                        relay = relay_url,
                        pubkey = %&pubkey[..12],
                        error = %e,
                        "crawler: relay fetch failed"
                    );
                }
            }

            // Delay between relay attempts
            if self.config.request_delay_ms > 0 {
                sleep(Duration::from_millis(self.config.request_delay_ms)).await;
            }
        }

        Some((total_new, oldest, newest))
    }

    /// Connect to a relay and fetch kind-1 notes for a specific author.
    async fn fetch_from_relay(
        &self,
        relay_url: &str,
        pubkey: &str,
        since: Option<i64>,
        until: Option<i64>,
    ) -> Result<Vec<NostrEvent>, String> {
        let connect_timeout = Duration::from_secs(10);
        let msg_timeout = Duration::from_secs(15);

        let (ws_stream, _) = timeout(connect_timeout, tokio_tungstenite::connect_async(relay_url))
            .await
            .map_err(|_| "connect timeout".to_string())?
            .map_err(|e| format!("connect failed: {e}"))?;

        let (mut write, mut read) = ws_stream.split();

        // Build filter
        let mut filter = serde_json::json!({
            "kinds": [1],
            "authors": [pubkey],
            "limit": self.config.events_per_author,
        });

        if let Some(s) = since {
            filter["since"] = serde_json::json!(s);
        }
        if let Some(u) = until {
            filter["until"] = serde_json::json!(u);
        }

        let sub_id = format!("crawl-{}", Uuid::new_v4().simple());
        let req = serde_json::json!(["REQ", &sub_id, filter]);

        write
            .send(Message::Text(req.to_string().into()))
            .await
            .map_err(|e| format!("send REQ failed: {e}"))?;

        let mut events = Vec::new();

        loop {
            match timeout(msg_timeout, read.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    let parsed: Value = match serde_json::from_str(&text) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    let arr = match parsed.as_array() {
                        Some(a) if a.len() >= 2 => a,
                        _ => continue,
                    };

                    match arr[0].as_str() {
                        Some("EVENT") if arr.len() >= 3 => {
                            if let Ok(event) = serde_json::from_value::<NostrEvent>(arr[2].clone())
                            {
                                if event.kind == 1 {
                                    events.push(event);
                                }
                            }
                        }
                        Some("EOSE") => break,
                        Some("CLOSED") | Some("NOTICE") => {
                            let msg = arr.get(1).and_then(|v| v.as_str()).unwrap_or("?");
                            tracing::debug!(relay = relay_url, msg, "relay message");
                            break;
                        }
                        _ => {}
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
}

fn merge_min(current: Option<i64>, new: Option<i64>) -> Option<i64> {
    match (current, new) {
        (Some(c), Some(n)) => Some(c.min(n)),
        (Some(c), None) => Some(c),
        (None, Some(n)) => Some(n),
        (None, None) => None,
    }
}

fn merge_max(current: Option<i64>, new: Option<i64>) -> Option<i64> {
    match (current, new) {
        (Some(c), Some(n)) => Some(c.max(n)),
        (Some(c), None) => Some(c),
        (None, Some(n)) => Some(n),
        (None, None) => None,
    }
}
