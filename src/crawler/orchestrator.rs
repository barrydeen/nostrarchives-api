use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use sqlx::PgPool;
use tokio::sync::broadcast;
use tokio::time::{interval, sleep, timeout};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use crate::cache::StatsCache;
use crate::db::models::NostrEvent;
use crate::db::repository::EventRepository;

use super::negentropy::NegentropySyncer;
use super::queue::CrawlQueue;
use super::relay_caps;
use super::relay_router::RelayRouter;

/// Configuration for the hybrid crawler.
#[derive(Debug, Clone)]
pub struct HybridCrawlerConfig {
    /// Whether negentropy sync is enabled.
    pub negentropy_enabled: bool,
    /// How often to run negentropy bulk sync (seconds).
    pub negentropy_sync_interval_secs: u64,
    /// Max relays to negentropy-sync per cycle.
    pub negentropy_max_relays: usize,
    /// Whether to use NIP-65 relay lists for targeted crawling.
    pub use_relay_lists: bool,
    /// Max concurrent relay connections for targeted crawling.
    pub max_relay_pool_size: usize,
    /// Legacy crawler config (kept for fallback).
    pub legacy_batch_size: i64,
    pub legacy_request_delay_ms: u64,
    pub legacy_poll_interval_secs: u64,
    pub legacy_events_per_author: i64,
    /// Fallback relay URLs when authors have no NIP-65 data.
    pub fallback_relay_urls: Vec<String>,
    /// Dry run mode: log what would be fetched but don't insert.
    pub dry_run: bool,
}

impl Default for HybridCrawlerConfig {
    fn default() -> Self {
        Self {
            negentropy_enabled: true,
            negentropy_sync_interval_secs: 300,
            negentropy_max_relays: 20,
            use_relay_lists: true,
            max_relay_pool_size: 50,
            legacy_batch_size: 10,
            legacy_request_delay_ms: 500,
            legacy_poll_interval_secs: 30,
            legacy_events_per_author: 500,
            fallback_relay_urls: vec![],
            dry_run: false,
        }
    }
}

/// Hybrid crawler that combines negentropy bulk sync with NIP-65 relay-list-aware
/// targeted crawling.
pub struct HybridCrawler {
    config: HybridCrawlerConfig,
    repo: EventRepository,
    cache: StatsCache,
    #[allow(dead_code)]
    pool: PgPool,
    queue: CrawlQueue,
    router: RelayRouter,
    total_events: Arc<AtomicU64>,
}

impl HybridCrawler {
    pub fn new(
        config: HybridCrawlerConfig,
        repo: EventRepository,
        cache: StatsCache,
        pool: PgPool,
        queue: CrawlQueue,
        router: RelayRouter,
    ) -> Self {
        Self {
            config,
            repo,
            cache,
            pool,
            queue,
            router,
            total_events: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Run the hybrid crawler until shutdown.
    pub async fn run(self, shutdown: broadcast::Sender<()>) {
        // 1. Initial relay discovery from NIP-65 data
        match self
            .router
            .get_top_relays(self.config.negentropy_max_relays as i64)
            .await
        {
            Ok(top_relays) => {
                if top_relays.is_empty() {
                    tracing::info!("hybrid crawler: no NIP-65 relay data found yet");
                } else {
                    tracing::info!(
                        relay_count = top_relays.len(),
                        "hybrid crawler: discovered top relays from NIP-65 data"
                    );
                    for (url, count) in &top_relays {
                        tracing::debug!(relay = %url, users = count, "discovered relay");
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "hybrid crawler: failed to query top relays");
            }
        }

        let this = Arc::new(self);

        // 2. Spawn negentropy bulk sync task (if enabled)
        let neg_handle = if this.config.negentropy_enabled {
            let crawler = Arc::clone(&this);
            let mut neg_shutdown = shutdown.subscribe();
            Some(tokio::spawn(async move {
                let mut tick = interval(Duration::from_secs(
                    crawler.config.negentropy_sync_interval_secs,
                ));
                // Skip the immediate first tick
                tick.tick().await;

                loop {
                    tokio::select! {
                        _ = tick.tick() => {
                            crawler.negentropy_sync_cycle().await;
                        }
                        _ = neg_shutdown.recv() => {
                            tracing::info!("hybrid crawler: negentropy sync shutting down");
                            return;
                        }
                    }
                }
            }))
        } else {
            tracing::info!("hybrid crawler: negentropy sync disabled");
            None
        };

        // 3. Spawn targeted crawl task (if use_relay_lists enabled)
        let crawl_handle = if this.config.use_relay_lists {
            let crawler = Arc::clone(&this);
            let mut crawl_shutdown = shutdown.subscribe();
            Some(tokio::spawn(async move {
                let mut tick =
                    interval(Duration::from_secs(crawler.config.legacy_poll_interval_secs));

                loop {
                    tokio::select! {
                        _ = tick.tick() => {
                            crawler.targeted_crawl_cycle().await;
                        }
                        _ = crawl_shutdown.recv() => {
                            tracing::info!("hybrid crawler: targeted crawl shutting down");
                            return;
                        }
                    }
                }
            }))
        } else {
            tracing::info!("hybrid crawler: relay-list-aware crawling disabled");
            None
        };

        // 4. Wait for shutdown
        let mut shutdown_rx = shutdown.subscribe();
        let _ = shutdown_rx.recv().await;
        tracing::info!("hybrid crawler: received shutdown signal");

        // Wait for spawned tasks to finish
        if let Some(h) = neg_handle {
            let _ = h.await;
        }
        if let Some(h) = crawl_handle {
            let _ = h.await;
        }

        tracing::info!(
            total_events = this.total_events.load(Ordering::Relaxed),
            "hybrid crawler: shutdown complete"
        );
    }

    /// Run one cycle of negentropy bulk sync.
    /// Discovers top relays, probes for NIP-77 support, and runs set reconciliation
    /// against capable relays.
    async fn negentropy_sync_cycle(&self) {
        let top_relays = match self
            .router
            .get_top_relays(self.config.negentropy_max_relays as i64)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "negentropy cycle: failed to get top relays");
                return;
            }
        };

        if top_relays.is_empty() {
            tracing::debug!("negentropy cycle: no relays to sync");
            return;
        }

        tracing::info!(
            relay_count = top_relays.len(),
            "negentropy cycle: starting bulk sync"
        );

        let syncer = NegentropySyncer::new(
            self.repo.clone(),
            self.cache.clone(),
            self.pool.clone(),
        );

        let mut total_discovered = 0usize;
        let mut total_inserted = 0usize;
        let mut relays_synced = 0usize;

        for (relay_url, user_count) in &top_relays {
            // Check relay capabilities (NIP-11 + NEG-OPEN probe with DB caching)
            let caps = match relay_caps::check_and_update_caps(&self.pool, relay_url).await {
                Ok(c) => c,
                Err(e) => {
                    tracing::debug!(
                        relay = %relay_url,
                        error = %e,
                        "negentropy cycle: capability check failed"
                    );
                    continue;
                }
            };

            if !caps.supports_negentropy {
                tracing::debug!(
                    relay = %relay_url,
                    users = user_count,
                    "negentropy cycle: relay does not support negentropy, skipping"
                );
                continue;
            }

            if self.config.dry_run {
                tracing::info!(
                    relay = %relay_url,
                    users = user_count,
                    "negentropy cycle: DRY RUN — would sync with relay"
                );
                continue;
            }

            // Run exhaustive sync: recent pass (all kinds) + backfill from cursor
            // max_windows_per_kind=5 means each kind walks back 5 days per cycle
            match syncer.run_exhaustive_sync(relay_url, 5).await {
                Ok(stats) => {
                    total_discovered += stats.events_discovered;
                    total_inserted += stats.events_inserted;
                    relays_synced += 1;
                    self.total_events
                        .fetch_add(stats.events_inserted as u64, Ordering::Relaxed);
                    tracing::info!(
                        relay = %relay_url,
                        discovered = stats.events_discovered,
                        inserted = stats.events_inserted,
                        duration_ms = stats.duration_ms,
                        "negentropy cycle: relay sync complete"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        relay = %relay_url,
                        error = %e,
                        "negentropy cycle: sync failed"
                    );
                }
            }

            // Brief delay between relays to be polite
            if self.config.legacy_request_delay_ms > 0 {
                sleep(Duration::from_millis(self.config.legacy_request_delay_ms)).await;
            }
        }

        tracing::info!(
            relays_synced = relays_synced,
            total_discovered = total_discovered,
            total_inserted = total_inserted,
            "negentropy cycle: complete"
        );
    }

    /// Run one cycle of relay-list-aware targeted crawling.
    async fn targeted_crawl_cycle(&self) {
        // Take a batch from the crawl queue
        let batch = match self.queue.take_batch(self.config.legacy_batch_size).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "targeted crawl: failed to take batch");
                return;
            }
        };

        if batch.is_empty() {
            return;
        }

        let pubkeys: Vec<String> = batch.iter().map(|t| t.pubkey.clone()).collect();

        tracing::info!(
            batch_size = batch.len(),
            "targeted crawl: processing batch"
        );

        // Group authors by their preferred relays
        let relay_groups = match self.router.get_relay_author_groups(&pubkeys).await {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!(error = %e, "targeted crawl: failed to get relay groups");
                std::collections::HashMap::new()
            }
        };

        // Track which authors got routed to a relay
        let mut routed_pubkeys: std::collections::HashSet<String> = std::collections::HashSet::new();

        // Crawl each relay group
        for (relay_url, group_pubkeys) in &relay_groups {
            tracing::info!(
                relay = %relay_url,
                authors = group_pubkeys.len(),
                "targeted crawl: fetching from relay"
            );

            for pk in group_pubkeys {
                routed_pubkeys.insert(pk.clone());
            }

            if self.config.dry_run {
                tracing::info!(
                    relay = %relay_url,
                    authors = group_pubkeys.len(),
                    "targeted crawl: DRY RUN — would crawl authors from relay"
                );
                continue;
            }

            match self
                .fetch_authors_from_relay(
                    relay_url,
                    group_pubkeys,
                    self.config.legacy_events_per_author,
                )
                .await
            {
                Ok(events) => {
                    let mut new_count = 0u64;
                    for event in &events {
                        match self.repo.insert_event(event, relay_url).await {
                            Ok(true) => {
                                new_count += 1;
                                self.cache
                                    .on_event_ingested(&event.pubkey, event.kind)
                                    .await;
                            }
                            Ok(false) => {} // duplicate
                            Err(e) => {
                                tracing::warn!(
                                    event_id = %event.id,
                                    error = %e,
                                    "targeted crawl: insert failed"
                                );
                            }
                        }
                    }
                    if new_count > 0 {
                        let total = self
                            .total_events
                            .fetch_add(new_count, Ordering::Relaxed)
                            + new_count;
                        tracing::info!(
                            relay = %relay_url,
                            fetched = events.len(),
                            new = new_count,
                            total = total,
                            "targeted crawl: relay batch complete"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        relay = %relay_url,
                        error = %e,
                        "targeted crawl: fetch failed"
                    );
                }
            }

            // Delay between relays
            if self.config.legacy_request_delay_ms > 0 {
                sleep(Duration::from_millis(self.config.legacy_request_delay_ms)).await;
            }
        }

        // Fall back to hardcoded relays for authors without NIP-65 data
        let unrouted: Vec<String> = pubkeys
            .iter()
            .filter(|pk| !routed_pubkeys.contains(*pk))
            .cloned()
            .collect();

        if !unrouted.is_empty() && !self.config.fallback_relay_urls.is_empty() {
            tracing::info!(
                authors = unrouted.len(),
                "targeted crawl: falling back to default relays for unrouted authors"
            );

            for relay_url in &self.config.fallback_relay_urls {
                if self.config.dry_run {
                    tracing::info!(
                        relay = %relay_url,
                        authors = unrouted.len(),
                        "targeted crawl: DRY RUN — would crawl unrouted authors"
                    );
                    continue;
                }

                match self
                    .fetch_authors_from_relay(
                        relay_url,
                        &unrouted,
                        self.config.legacy_events_per_author,
                    )
                    .await
                {
                    Ok(events) => {
                        let mut new_count = 0u64;
                        for event in &events {
                            match self.repo.insert_event(event, relay_url).await {
                                Ok(true) => {
                                    new_count += 1;
                                    self.cache
                                        .on_event_ingested(&event.pubkey, event.kind)
                                        .await;
                                }
                                Ok(false) => {}
                                Err(e) => {
                                    tracing::warn!(
                                        event_id = %event.id,
                                        error = %e,
                                        "targeted crawl: fallback insert failed"
                                    );
                                }
                            }
                        }
                        if new_count > 0 {
                            let total = self
                                .total_events
                                .fetch_add(new_count, Ordering::Relaxed)
                                + new_count;
                            tracing::info!(
                                relay = %relay_url,
                                new = new_count,
                                total = total,
                                "targeted crawl: fallback relay complete"
                            );
                        }
                        // Got results from one fallback relay, stop
                        if !events.is_empty() {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::debug!(
                            relay = %relay_url,
                            error = %e,
                            "targeted crawl: fallback fetch failed"
                        );
                    }
                }

                if self.config.legacy_request_delay_ms > 0 {
                    sleep(Duration::from_millis(self.config.legacy_request_delay_ms)).await;
                }
            }
        }

        // Update crawl state for all authors in the batch
        for target in &batch {
            // Mark as crawled even if we didn't find new events — the queue
            // will schedule the next crawl based on priority tier.
            if let Err(e) = self.queue.mark_crawled(&target.pubkey, 0, None, None).await {
                tracing::warn!(
                    pubkey = %target.pubkey,
                    error = %e,
                    "targeted crawl: failed to update crawl state"
                );
            }
        }
    }

    /// Fetch kind-1 notes for specific authors from a specific relay.
    /// Sends a single REQ with all authors batched together.
    async fn fetch_authors_from_relay(
        &self,
        relay_url: &str,
        pubkeys: &[String],
        events_per_author: i64,
    ) -> Result<Vec<NostrEvent>, String> {
        if pubkeys.is_empty() {
            return Ok(vec![]);
        }

        let connect_timeout = Duration::from_secs(10);
        let msg_timeout = Duration::from_secs(15);

        let (ws_stream, _) = timeout(connect_timeout, tokio_tungstenite::connect_async(relay_url))
            .await
            .map_err(|_| format!("connect timeout: {relay_url}"))?
            .map_err(|e| format!("connect failed to {relay_url}: {e}"))?;

        let (mut write, mut read) = ws_stream.split();

        // Batch authors into a single filter (relays support author arrays)
        let limit = (events_per_author * pubkeys.len() as i64).min(5000);
        let filter = serde_json::json!({
            "kinds": [1],
            "authors": pubkeys,
            "limit": limit,
        });

        let sub_id = format!("hc-{}", Uuid::new_v4().simple());
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
                            if let Ok(event) =
                                serde_json::from_value::<NostrEvent>(arr[2].clone())
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
