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
use super::queue::{CrawlQueue, CrawlTarget};
use super::relay_caps;
use super::relay_router::RelayRouter;

/// Per-author crawl statistics tracked during a targeted crawl cycle.
struct AuthorCrawlStats {
    new_events: i64,
    oldest_ts: Option<i64>,
    newest_ts: Option<i64>,
}

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
    /// Primary negentropy relay URLs tried for ALL authors before their specific write relays.
    /// If empty, falls back to all DB-known negentropy relays.
    pub primary_negentropy_relay_urls: Vec<String>,
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
            primary_negentropy_relay_urls: vec![],
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

        // 1b. Seed the crawl queue from follows/WoT
        match self.queue.sync_from_follows().await {
            Ok(stats) => {
                tracing::info!(
                    new = stats.new_pubkeys,
                    updated = stats.updated_counts,
                    "hybrid crawler: initial queue sync complete"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "hybrid crawler: initial queue sync failed");
            }
        }
        if let Ok(n) = self.queue.sync_cursors_from_events().await {
            if n > 0 {
                tracing::info!(updated = n, "hybrid crawler: synced cursors from existing events");
            }
        }

        let this = Arc::new(self);

        // 2a. Spawn periodic queue re-sync (every 5 minutes)
        {
            let queue = this.queue.clone();
            tokio::spawn(async move {
                let mut tick = interval(Duration::from_secs(300));
                tick.tick().await; // skip immediate
                loop {
                    tick.tick().await;
                    match queue.sync_from_follows().await {
                        Ok(stats) if stats.new_pubkeys > 0 => {
                            tracing::info!(
                                new = stats.new_pubkeys,
                                updated = stats.updated_counts,
                                "hybrid crawler: queue re-sync"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "hybrid crawler: queue re-sync failed");
                        }
                        _ => {}
                    }
                }
            });
        }

        // 2b. Spawn negentropy bulk sync task (if enabled)
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
                    let err_msg = e.to_string();
                    tracing::warn!(
                        relay = %relay_url,
                        error = %err_msg,
                        "negentropy cycle: sync failed"
                    );
                    // If the relay doesn't actually support negentropy, mark it
                    // so we don't waste time retrying every cycle
                    if err_msg.contains("does not support negentropy")
                        || err_msg.contains("negentropy disabled")
                    {
                        let caps = relay_caps::RelayCaps {
                            relay_url: relay_url.clone(),
                            supports_negentropy: false,
                            max_limit: None,
                            nip11: None,
                            last_checked_at: chrono::Utc::now(),
                        };
                        let _ = relay_caps::upsert_relay_caps(&self.pool, &caps).await;
                        tracing::info!(
                            relay = %relay_url,
                            "negentropy cycle: marked relay as non-negentropy"
                        );
                    }
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

    /// Get primary negentropy relay URLs for per-author syncs.
    /// Uses explicitly configured URLs if set; otherwise falls back to all
    /// DB-known negentropy-capable relays discovered during capability probing.
    async fn primary_negentropy_relays(&self) -> Vec<String> {
        if !self.config.primary_negentropy_relay_urls.is_empty() {
            return self.config.primary_negentropy_relay_urls.clone();
        }
        relay_caps::get_negentropy_relays(&self.pool)
            .await
            .unwrap_or_default()
    }

    /// Run one cycle of relay-list-aware targeted crawling.
    /// Three phases per cycle:
    ///   1) Primary negentropy relays — all authors in batch, exhaustive
    ///   2) Author-specific write relays (NIP-65) — additional niche coverage
    ///   3) Fallback for unrouted authors (no NIP-65 data)
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

        // Build a map of pubkey -> crawl target for cursor lookups
        let target_map: std::collections::HashMap<String, &CrawlTarget> =
            batch.iter().map(|t| (t.pubkey.clone(), t)).collect();

        let pubkeys: Vec<String> = batch.iter().map(|t| t.pubkey.clone()).collect();

        tracing::info!(
            batch_size = batch.len(),
            "targeted crawl: processing batch"
        );

        // Track per-author stats across all relays
        let mut author_stats: std::collections::HashMap<String, AuthorCrawlStats> =
            std::collections::HashMap::new();

        // Track which authors have NIP-65 relay lists (for unrouted fallback detection)
        let mut routed_pubkeys: std::collections::HashSet<String> = std::collections::HashSet::new();

        // ── Phase 1: Primary negentropy relays — ALL authors ────────────────────
        // Always try big, globally-known negentropy-capable relays first for every
        // author in the batch, regardless of their NIP-65 data. This is the most
        // exhaustive strategy: these relays typically have the widest event coverage.
        // t1/t2 authors especially benefit from this breadth-first approach.
        let primary_neg_relays = self.primary_negentropy_relays().await;
        let primary_neg_set: std::collections::HashSet<String> =
            primary_neg_relays.iter().cloned().collect();

        if !primary_neg_relays.is_empty() {
            tracing::info!(
                relay_count = primary_neg_relays.len(),
                authors = pubkeys.len(),
                "targeted crawl: phase 1 — negentropy sync on primary relays"
            );

            for relay_url in &primary_neg_relays {
                if self.config.dry_run {
                    tracing::info!(
                        relay = %relay_url,
                        authors = pubkeys.len(),
                        "targeted crawl: DRY RUN — would negentropy sync all authors on primary relay"
                    );
                    continue;
                }

                let relay_has_neg = relay_caps::check_and_update_caps(&self.pool, relay_url)
                    .await
                    .map(|c| c.supports_negentropy)
                    .unwrap_or(false);

                if !relay_has_neg {
                    tracing::debug!(
                        relay = %relay_url,
                        "targeted crawl: primary relay has no negentropy, skipping"
                    );
                    continue;
                }

                let syncer = NegentropySyncer::new(
                    self.repo.clone(),
                    self.cache.clone(),
                    self.pool.clone(),
                );

                tracing::info!(
                    relay = %relay_url,
                    authors = pubkeys.len(),
                    "targeted crawl: phase 1 negentropy sync"
                );

                'phase1_chunks: for chunk in pubkeys.chunks(10) {
                    let chunk_vec: Vec<String> = chunk.to_vec();
                    match syncer.sync_authors(relay_url, &[1], &chunk_vec).await {
                        Ok(stats) => {
                            if stats.events_inserted > 0 {
                                self.total_events
                                    .fetch_add(stats.events_inserted as u64, Ordering::Relaxed);
                                tracing::info!(
                                    relay = %relay_url,
                                    discovered = stats.events_discovered,
                                    inserted = stats.events_inserted,
                                    duration_ms = stats.duration_ms,
                                    "targeted crawl: phase 1 negentropy complete"
                                );

                                // Fetch engagement for newly inserted notes
                                if let Ok(note_rows) = sqlx::query_as::<_, (String,)>(
                                    "SELECT id FROM events WHERE pubkey = ANY($1) AND kind = 1 ORDER BY created_at DESC LIMIT 200",
                                )
                                .bind(&chunk_vec)
                                .fetch_all(&self.pool)
                                .await
                                {
                                    let note_ids: Vec<String> =
                                        note_rows.into_iter().map(|r| r.0).collect();
                                    if !note_ids.is_empty() {
                                        match self
                                            .fetch_engagement_for_notes(relay_url, &note_ids)
                                            .await
                                        {
                                            Ok(n) if n > 0 => {
                                                tracing::info!(
                                                    relay = %relay_url,
                                                    engagement_events = n,
                                                    "targeted crawl: phase 1 engagement backfill"
                                                );
                                            }
                                            Err(e) => {
                                                tracing::debug!(
                                                    relay = %relay_url,
                                                    error = %e,
                                                    "targeted crawl: phase 1 engagement backfill failed"
                                                );
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                            }

                            // Fetch zaps received by these authors on this relay
                            match self.fetch_zaps_for_authors(relay_url, chunk).await {
                                Ok(n) if n > 0 => {
                                    tracing::info!(
                                        relay = %relay_url,
                                        authors = chunk.len(),
                                        zaps = n,
                                        "targeted crawl: phase 1 zap backfill"
                                    );
                                }
                                Err(e) => {
                                    tracing::debug!(
                                        relay = %relay_url,
                                        error = %e,
                                        "targeted crawl: phase 1 zap backfill failed"
                                    );
                                }
                                _ => {}
                            }

                            // Update per-author stats from DB
                            for pk in chunk {
                                if let Ok(row) = sqlx::query_as::<_, (i64, i64, i64)>(
                                    "SELECT count(*), COALESCE(min(created_at), 0), COALESCE(max(created_at), 0) FROM events WHERE pubkey = $1 AND kind = 1",
                                )
                                .bind(pk)
                                .fetch_one(&self.pool)
                                .await
                                {
                                    let entry =
                                        author_stats.entry(pk.clone()).or_insert(AuthorCrawlStats {
                                            new_events: 0,
                                            oldest_ts: None,
                                            newest_ts: None,
                                        });
                                    if row.1 > 0 {
                                        entry.oldest_ts = Some(row.1);
                                    }
                                    if row.2 > 0 {
                                        entry.newest_ts = Some(row.2);
                                    }
                                    entry.new_events = entry.new_events.max(0);
                                }
                            }
                        }
                        Err(e) => {
                            let err_msg = e.to_string();
                            tracing::warn!(
                                relay = %relay_url,
                                error = %err_msg,
                                "targeted crawl: phase 1 negentropy failed"
                            );
                            // Mark relay as non-negentropy and skip remaining chunks
                            if err_msg.contains("does not support negentropy")
                                || err_msg.contains("negentropy disabled")
                            {
                                let caps = relay_caps::RelayCaps {
                                    relay_url: relay_url.clone(),
                                    supports_negentropy: false,
                                    max_limit: None,
                                    nip11: None,
                                    last_checked_at: chrono::Utc::now(),
                                };
                                let _ = relay_caps::upsert_relay_caps(&self.pool, &caps).await;
                                tracing::info!(
                                    relay = %relay_url,
                                    "targeted crawl: phase 1 relay marked as non-negentropy"
                                );
                                break 'phase1_chunks;
                            }
                        }
                    }

                    if self.config.legacy_request_delay_ms > 0 {
                        sleep(Duration::from_millis(self.config.legacy_request_delay_ms)).await;
                    }
                }

                if self.config.legacy_request_delay_ms > 0 {
                    sleep(Duration::from_millis(self.config.legacy_request_delay_ms)).await;
                }
            }
        }

        // ── Phase 2: Author-specific write relays (NIP-65) ──────────────────────
        // Additional coverage from each author's own write relays, which may hold
        // content not yet on the primary relays (niche or personal relays).
        // Relays already covered exhaustively in Phase 1 are skipped.
        let relay_groups = match self.router.get_relay_author_groups(&pubkeys).await {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!(error = %e, "targeted crawl: failed to get relay groups");
                std::collections::HashMap::new()
            }
        };

        for (relay_url, group_pubkeys) in &relay_groups {
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

            // Skip relays already covered exhaustively in Phase 1
            if primary_neg_set.contains(relay_url) {
                tracing::debug!(
                    relay = %relay_url,
                    authors = group_pubkeys.len(),
                    "targeted crawl: relay covered in phase 1, skipping phase 2"
                );
                continue;
            }

            // Check if relay supports negentropy (cached in DB)
            let relay_has_neg = relay_caps::check_and_update_caps(&self.pool, relay_url)
                .await
                .map(|c| c.supports_negentropy)
                .unwrap_or(false);

            if relay_has_neg {
                // ── Negentropy path: full set reconciliation per author ──
                let syncer = NegentropySyncer::new(
                    self.repo.clone(),
                    self.cache.clone(),
                    self.pool.clone(),
                );

                tracing::info!(
                    relay = %relay_url,
                    authors = group_pubkeys.len(),
                    "targeted crawl: phase 2 negentropy sync"
                );

                // Sync in sub-batches of 10 authors to keep negentropy sets manageable
                for chunk in group_pubkeys.chunks(10) {
                    let chunk_vec: Vec<String> = chunk.to_vec();
                    match syncer.sync_authors(relay_url, &[1], &chunk_vec).await {
                        Ok(stats) => {
                            if stats.events_inserted > 0 {
                                let total = self
                                    .total_events
                                    .fetch_add(stats.events_inserted as u64, Ordering::Relaxed)
                                    + stats.events_inserted as u64;
                                tracing::info!(
                                    relay = %relay_url,
                                    discovered = stats.events_discovered,
                                    inserted = stats.events_inserted,
                                    total = total,
                                    duration_ms = stats.duration_ms,
                                    "targeted crawl: phase 2 negentropy complete"
                                );

                                if let Ok(note_rows) = sqlx::query_as::<_, (String,)>(
                                    "SELECT id FROM events WHERE pubkey = ANY($1) AND kind = 1 ORDER BY created_at DESC LIMIT 200",
                                )
                                .bind(&chunk_vec)
                                .fetch_all(&self.pool)
                                .await
                                {
                                    let note_ids: Vec<String> =
                                        note_rows.into_iter().map(|r| r.0).collect();
                                    if !note_ids.is_empty() {
                                        match self
                                            .fetch_engagement_for_notes(relay_url, &note_ids)
                                            .await
                                        {
                                            Ok(n) if n > 0 => {
                                                tracing::info!(
                                                    relay = %relay_url,
                                                    engagement_events = n,
                                                    "targeted crawl: phase 2 engagement backfill"
                                                );
                                            }
                                            Err(e) => {
                                                tracing::debug!(
                                                    relay = %relay_url,
                                                    error = %e,
                                                    "targeted crawl: phase 2 engagement backfill failed"
                                                );
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                            }

                            match self.fetch_zaps_for_authors(relay_url, chunk).await {
                                Ok(n) if n > 0 => {
                                    tracing::info!(
                                        relay = %relay_url,
                                        authors = chunk.len(),
                                        zaps = n,
                                        "targeted crawl: phase 2 zap backfill"
                                    );
                                }
                                Err(e) => {
                                    tracing::debug!(
                                        relay = %relay_url,
                                        error = %e,
                                        "targeted crawl: phase 2 zap backfill failed"
                                    );
                                }
                                _ => {}
                            }

                            for pk in chunk {
                                if let Ok(row) = sqlx::query_as::<_, (i64, i64, i64)>(
                                    "SELECT count(*), COALESCE(min(created_at), 0), COALESCE(max(created_at), 0) FROM events WHERE pubkey = $1 AND kind = 1",
                                )
                                .bind(pk)
                                .fetch_one(&self.pool)
                                .await
                                {
                                    let entry =
                                        author_stats.entry(pk.clone()).or_insert(AuthorCrawlStats {
                                            new_events: 0,
                                            oldest_ts: None,
                                            newest_ts: None,
                                        });
                                    if row.1 > 0 {
                                        entry.oldest_ts = Some(row.1);
                                    }
                                    if row.2 > 0 {
                                        entry.newest_ts = Some(row.2);
                                    }
                                    entry.new_events = entry.new_events.max(0);
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                relay = %relay_url,
                                error = %e,
                                "targeted crawl: phase 2 negentropy failed, falling back to legacy"
                            );
                            // Fall back to legacy REQ for this chunk
                            self.legacy_crawl_authors(
                                relay_url,
                                chunk,
                                &target_map,
                                &mut author_stats,
                            )
                            .await;
                        }
                    }

                    if self.config.legacy_request_delay_ms > 0 {
                        sleep(Duration::from_millis(self.config.legacy_request_delay_ms)).await;
                    }
                }
            } else {
                // ── Legacy REQ path: for relays without negentropy ──
                self.legacy_crawl_authors(
                    relay_url,
                    group_pubkeys,
                    &target_map,
                    &mut author_stats,
                )
                .await;
            }
        }

        // ── Phase 3: Fallback for unrouted authors (no NIP-65 data) ─────────────
        // Authors without NIP-65 relay lists were already covered by Phase 1.
        // Only use fallback relays that weren't already in the primary negentropy
        // set to avoid redundant syncs. Legacy REQ is the last resort here.
        let unrouted: Vec<String> = pubkeys
            .iter()
            .filter(|pk| !routed_pubkeys.contains(*pk))
            .cloned()
            .collect();

        if !unrouted.is_empty() && !self.config.fallback_relay_urls.is_empty() {
            tracing::info!(
                authors = unrouted.len(),
                "targeted crawl: phase 3 — fallback for unrouted authors"
            );

            for relay_url in &self.config.fallback_relay_urls {
                // Skip relays already covered in Phase 1
                if primary_neg_set.contains(relay_url) {
                    continue;
                }

                if self.config.dry_run {
                    tracing::info!(
                        relay = %relay_url,
                        authors = unrouted.len(),
                        "targeted crawl: DRY RUN — would crawl unrouted authors"
                    );
                    continue;
                }

                let relay_has_neg = relay_caps::check_and_update_caps(&self.pool, relay_url)
                    .await
                    .map(|c| c.supports_negentropy)
                    .unwrap_or(false);

                if relay_has_neg {
                    let syncer = NegentropySyncer::new(
                        self.repo.clone(),
                        self.cache.clone(),
                        self.pool.clone(),
                    );

                    for chunk in unrouted.chunks(10) {
                        let chunk_vec: Vec<String> = chunk.to_vec();
                        match syncer.sync_authors(relay_url, &[1], &chunk_vec).await {
                            Ok(stats) => {
                                if stats.events_inserted > 0 {
                                    self.total_events
                                        .fetch_add(stats.events_inserted as u64, Ordering::Relaxed);
                                    tracing::info!(
                                        relay = %relay_url,
                                        inserted = stats.events_inserted,
                                        "targeted crawl: phase 3 negentropy complete"
                                    );
                                }
                            }
                            Err(e) => {
                                tracing::debug!(
                                    relay = %relay_url,
                                    error = %e,
                                    "targeted crawl: phase 3 negentropy failed"
                                );
                            }
                        }
                    }
                    match self.fetch_zaps_for_authors(relay_url, &unrouted).await {
                        Ok(n) if n > 0 => {
                            tracing::info!(
                                relay = %relay_url,
                                authors = unrouted.len(),
                                zaps = n,
                                "targeted crawl: phase 3 zap backfill"
                            );
                        }
                        Err(e) => {
                            tracing::debug!(relay = %relay_url, error = %e, "targeted crawl: phase 3 zap backfill failed");
                        }
                        _ => {}
                    }
                    break; // negentropy gives complete diff, no need for more relays
                } else {
                    self.legacy_crawl_authors(
                        relay_url,
                        &unrouted,
                        &target_map,
                        &mut author_stats,
                    )
                    .await;
                }

                if self.config.legacy_request_delay_ms > 0 {
                    sleep(Duration::from_millis(self.config.legacy_request_delay_ms)).await;
                }
            }
        }

        // Update crawl state for all authors in the batch with actual per-author stats
        for target in &batch {
            let stats = author_stats.get(&target.pubkey);
            let notes_found = stats.map_or(0, |s| s.new_events);
            let oldest = stats.and_then(|s| s.oldest_ts);
            let newest = stats.and_then(|s| s.newest_ts);
            if let Err(e) = self
                .queue
                .mark_crawled(&target.pubkey, notes_found, oldest, newest)
                .await
            {
                tracing::warn!(
                    pubkey = %target.pubkey,
                    error = %e,
                    "targeted crawl: failed to update crawl state"
                );
            }
        }
    }

    /// Ingest events into the database and track per-author stats.
    /// Returns (new_insert_count, new_note_ids_for_engagement).
    async fn ingest_events(
        &self,
        events: &[NostrEvent],
        relay_url: &str,
        author_stats: &mut std::collections::HashMap<String, AuthorCrawlStats>,
    ) -> (u64, Vec<String>) {
        let mut new_count = 0u64;
        let mut new_note_ids = Vec::new();

        for event in events {
            match self.repo.insert_event(event, relay_url).await {
                Ok(true) => {
                    new_count += 1;
                    self.cache
                        .on_event_ingested(&event.pubkey, event.kind)
                        .await;
                    if event.kind == 1 {
                        new_note_ids.push(event.id.clone());
                    }
                    // Update per-author stats
                    let entry = author_stats.entry(event.pubkey.clone()).or_insert(AuthorCrawlStats {
                        new_events: 0,
                        oldest_ts: None,
                        newest_ts: None,
                    });
                    entry.new_events += 1;
                    entry.oldest_ts = Some(match entry.oldest_ts {
                        Some(old) if old < event.created_at => old,
                        _ => event.created_at,
                    });
                    entry.newest_ts = Some(match entry.newest_ts {
                        Some(new) if new > event.created_at => new,
                        _ => event.created_at,
                    });
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

        (new_count, new_note_ids)
    }

    /// Legacy REQ-based crawl for a group of authors on a single relay.
    /// Used when the relay doesn't support negentropy, or as fallback on negentropy failure.
    /// Runs Phase A (recent notes since newest_seen_at) + Phase B (historical backfill).
    async fn legacy_crawl_authors(
        &self,
        relay_url: &str,
        group_pubkeys: &[String],
        target_map: &std::collections::HashMap<String, &CrawlTarget>,
        author_stats: &mut std::collections::HashMap<String, AuthorCrawlStats>,
    ) {
        // Phase A: Recent notes (since newest_seen_at)
        let all_have_cursor = group_pubkeys.iter().all(|pk| {
            target_map.get(pk).map_or(false, |t| t.newest_seen_at.is_some())
        });
        let since_ts = if all_have_cursor {
            group_pubkeys.iter().filter_map(|pk| {
                target_map.get(pk).and_then(|t| t.newest_seen_at)
            }).min()
        } else {
            None
        };

        tracing::info!(
            relay = %relay_url,
            authors = group_pubkeys.len(),
            since = ?since_ts,
            "targeted crawl: legacy phase A (recent)"
        );

        match self
            .fetch_authors_from_relay(
                relay_url,
                group_pubkeys,
                self.config.legacy_events_per_author,
                since_ts,
                None,
            )
            .await
        {
            Ok(events) => {
                let (new_count, new_note_ids) = self.ingest_events(&events, relay_url, author_stats).await;
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
                        "targeted crawl: legacy recent batch complete"
                    );
                }
                if !new_note_ids.is_empty() {
                    if let Ok(n) = self.fetch_engagement_for_notes(relay_url, &new_note_ids).await {
                        if n > 0 {
                            tracing::info!(
                                relay = %relay_url,
                                engagement_events = n,
                                "targeted crawl: engagement backfill complete"
                            );
                        }
                    }
                }

                // Fetch zaps received by these authors (#p tag query)
                match self.fetch_zaps_for_authors(relay_url, group_pubkeys).await {
                    Ok(n) if n > 0 => {
                        tracing::info!(
                            relay = %relay_url,
                            authors = group_pubkeys.len(),
                            zaps = n,
                            "targeted crawl: legacy zap backfill complete"
                        );
                    }
                    Err(e) => {
                        tracing::debug!(relay = %relay_url, error = %e, "targeted crawl: legacy zap backfill failed");
                    }
                    _ => {}
                }
            }
            Err(e) => {
                tracing::warn!(
                    relay = %relay_url,
                    error = %e,
                    "targeted crawl: legacy recent fetch failed"
                );
            }
        }

        if self.config.legacy_request_delay_ms > 0 {
            sleep(Duration::from_millis(self.config.legacy_request_delay_ms)).await;
        }

        // Phase B: Historical backfill — per-author with individual cursors.
        // Tiers 1-3 are included; Tier 4 (< 10 followers) is skipped.
        // Each author uses their own crawl_cursor so mixed-age authors don't starve each other.
        for pk in group_pubkeys {
            let target = match target_map.get(pk) {
                Some(t) => t,
                None => continue,
            };
            if target.priority_tier > 3 {
                continue;
            }
            let until_ts = match target.crawl_cursor {
                Some(ts) => ts,
                None => continue,
            };

            tracing::debug!(
                relay = %relay_url,
                pubkey = %pk,
                until = until_ts,
                tier = target.priority_tier,
                "targeted crawl: legacy phase B (backfill)"
            );

            match self
                .fetch_authors_from_relay(
                    relay_url,
                    std::slice::from_ref(pk),
                    self.config.legacy_events_per_author,
                    None,
                    Some(until_ts),
                )
                .await
            {
                Ok(events) => {
                    let (new_count, new_note_ids) =
                        self.ingest_events(&events, relay_url, author_stats).await;
                    if new_count > 0 {
                        let total = self
                            .total_events
                            .fetch_add(new_count, Ordering::Relaxed)
                            + new_count;
                        tracing::info!(
                            relay = %relay_url,
                            pubkey = %pk,
                            new = new_count,
                            total = total,
                            "targeted crawl: legacy backfill complete"
                        );
                    }
                    if !new_note_ids.is_empty() {
                        if let Ok(n) =
                            self.fetch_engagement_for_notes(relay_url, &new_note_ids).await
                        {
                            if n > 0 {
                                tracing::info!(
                                    relay = %relay_url,
                                    engagement_events = n,
                                    "targeted crawl: historical engagement complete"
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        relay = %relay_url,
                        pubkey = %pk,
                        error = %e,
                        "targeted crawl: legacy backfill fetch failed"
                    );
                }
            }

            if self.config.legacy_request_delay_ms > 0 {
                sleep(Duration::from_millis(self.config.legacy_request_delay_ms)).await;
            }
        }
    }

    /// Phase B: Fetch engagement (reactions, reposts, zaps) for recently crawled notes.
    /// Queries relays with `{"kinds": [6, 7, 16, 9735], "#e": [batch of note IDs]}`.
    /// Reactions/reposts are processed as counter increments, zaps are fully stored.
    async fn fetch_engagement_for_notes(
        &self,
        relay_url: &str,
        note_ids: &[String],
    ) -> Result<u64, String> {
        if note_ids.is_empty() {
            return Ok(0);
        }

        let connect_timeout = Duration::from_secs(10);
        let msg_timeout = Duration::from_secs(15);

        let (ws_stream, _) = timeout(connect_timeout, tokio_tungstenite::connect_async(relay_url))
            .await
            .map_err(|_| format!("connect timeout: {relay_url}"))?
            .map_err(|e| format!("connect failed to {relay_url}: {e}"))?;

        let (mut write, mut read) = ws_stream.split();

        // Batch note IDs into chunks of 50 to avoid relay filter limits
        let mut total_processed = 0u64;

        for chunk in note_ids.chunks(50) {
            let filter = serde_json::json!({
                "kinds": [6, 7, 16, 9735],
                "#e": chunk,
                "limit": 5000,
            });

            let sub_id = format!("eng-{}", Uuid::new_v4().simple());
            let req = serde_json::json!(["REQ", &sub_id, filter]);

            write
                .send(Message::Text(req.to_string().into()))
                .await
                .map_err(|e| format!("send REQ failed: {e}"))?;

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
                                    // Process through insert_event which handles counter logic
                                    match self.repo.insert_event(&event, relay_url).await {
                                        Ok(true) => {
                                            total_processed += 1;
                                            self.cache
                                                .on_event_ingested(&event.pubkey, event.kind)
                                                .await;
                                        }
                                        Ok(false) => {} // duplicate or skipped
                                        Err(e) => {
                                            tracing::debug!(
                                                error = %e,
                                                "engagement backfill: insert failed"
                                            );
                                        }
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

            // Close subscription
            let close_msg = serde_json::json!(["CLOSE", &sub_id]);
            let _ = write
                .send(Message::Text(close_msg.to_string().into()))
                .await;
        }

        Ok(total_processed)
    }

    /// Fetch zap receipts (kind 9735) received by specific authors from a relay.
    /// Uses `#p` tag filter because zaps are authored by the LNURL provider,
    /// not the note author — so negentropy by-author doesn't find them.
    async fn fetch_zaps_for_authors(
        &self,
        relay_url: &str,
        pubkeys: &[String],
    ) -> Result<u64, String> {
        if pubkeys.is_empty() {
            return Ok(0);
        }

        let connect_timeout = Duration::from_secs(10);
        let msg_timeout = Duration::from_secs(20);

        let (ws_stream, _) = timeout(connect_timeout, tokio_tungstenite::connect_async(relay_url))
            .await
            .map_err(|_| format!("connect timeout: {relay_url}"))?
            .map_err(|e| format!("connect failed to {relay_url}: {e}"))?;

        let (mut write, mut read) = ws_stream.split();
        let mut total_inserted = 0u64;

        // Chunk into batches of 50 authors to stay within relay filter limits
        for chunk in pubkeys.chunks(50) {
            let filter = serde_json::json!({
                "kinds": [9735],
                "#p": chunk,
                "limit": 5000,
            });

            let sub_id = format!("zaps-{}", Uuid::new_v4().simple());
            let req = serde_json::json!(["REQ", &sub_id, filter]);

            write
                .send(Message::Text(req.to_string().into()))
                .await
                .map_err(|e| format!("send REQ failed: {e}"))?;

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
                                    if event.kind == 9735 {
                                        match self.repo.insert_event(&event, relay_url).await {
                                            Ok(true) => {
                                                total_inserted += 1;
                                                self.cache
                                                    .on_event_ingested(&event.pubkey, event.kind)
                                                    .await;
                                            }
                                            Ok(false) => {}
                                            Err(e) => {
                                                tracing::debug!(
                                                    error = %e,
                                                    "fetch_zaps_for_authors: insert failed"
                                                );
                                            }
                                        }
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
        }

        Ok(total_inserted)
    }

    /// Fetch kind-1 notes for specific authors from a specific relay.
    /// Supports optional `since`/`until` for cursor-based pagination.
    async fn fetch_authors_from_relay(
        &self,
        relay_url: &str,
        pubkeys: &[String],
        events_per_author: i64,
        since: Option<i64>,
        until: Option<i64>,
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
        let mut filter = serde_json::json!({
            "kinds": [1],
            "authors": pubkeys,
            "limit": limit,
        });
        if let Some(s) = since {
            filter["since"] = serde_json::json!(s);
        }
        if let Some(u) = until {
            filter["until"] = serde_json::json!(u);
        }

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
