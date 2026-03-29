use std::sync::Arc;
use std::time::Instant;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use sqlx::PgPool;
use tokio::sync::broadcast;
use tokio::time::{sleep, timeout, Duration};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use crate::cache::StatsCache;
use crate::db::models::NostrEvent;
use crate::db::repository::EventRepository;

use super::negentropy::NegentropySyncer;
use super::queue::CrawlQueue;
use super::relay_caps;
use super::relay_router::RelayRouter;

/// Simple negentropy-only crawler: iterates through every author in crawl_state
/// and does a negentropy sync of their kind-1 notes against a pinned set of relays.
/// After the note pass, runs a zap crawl pass that fetches kind-9735 zap receipts
/// for all authors across the top relays discovered from NIP-65 data.
pub struct NegentropyOnlyCrawler {
    repo: EventRepository,
    cache: StatsCache,
    pool: PgPool,
    queue: CrawlQueue,
    pinned_relays: Vec<String>,
    confirmed_relays: Vec<String>,
    relay_router: RelayRouter,
}

impl NegentropyOnlyCrawler {
    pub fn new(
        repo: EventRepository,
        cache: StatsCache,
        pool: PgPool,
        queue: CrawlQueue,
        pinned_relays: Vec<String>,
        relay_router: RelayRouter,
    ) -> Self {
        Self {
            repo,
            cache,
            pool,
            queue,
            pinned_relays,
            confirmed_relays: Vec::new(),
            relay_router,
        }
    }

    /// Test each pinned relay for negentropy support with author filter.
    /// Only relays that pass are kept in confirmed_relays.
    pub async fn validate_relays(&mut self) {
        tracing::info!(
            relay_count = self.pinned_relays.len(),
            "negentropy_only: validating pinned relays"
        );

        let mut confirmed = Vec::new();

        for relay_url in &self.pinned_relays {
            // Use basic NEG-OPEN probe (no author filter) since the author-filtered
            // probe can give false negatives when the relay responds with an immediate
            // empty reconciliation for a dummy pubkey. Per-author syncs with real
            // pubkeys work fine at runtime; errors are handled gracefully per-relay.
            match relay_caps::probe_neg_open(relay_url).await {
                Ok(true) => {
                    tracing::info!(
                        relay = %relay_url,
                        "negentropy_only: relay confirmed (negentropy support)"
                    );
                    confirmed.push(relay_url.clone());
                }
                Ok(false) => {
                    tracing::warn!(
                        relay = %relay_url,
                        "negentropy_only: relay does NOT support negentropy"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        relay = %relay_url,
                        error = %e,
                        "negentropy_only: relay probe failed"
                    );
                }
            }
        }

        tracing::info!(
            confirmed = confirmed.len(),
            total = self.pinned_relays.len(),
            "negentropy_only: relay validation complete"
        );

        self.confirmed_relays = confirmed;
    }

    /// Run the negentropy-only crawl loop until shutdown.
    pub async fn run(self, shutdown: broadcast::Sender<()>) {
        if self.confirmed_relays.is_empty() {
            tracing::error!("negentropy_only: no confirmed relays, cannot crawl");
            return;
        }

        // Seed the crawl queue from follows
        match self.queue.sync_from_follows().await {
            Ok(stats) => {
                tracing::info!(
                    new = stats.new_pubkeys,
                    updated = stats.updated_counts,
                    "negentropy_only: queue seeded from follows"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "negentropy_only: queue seed failed");
            }
        }

        // Count authors needing note crawl
        let total_authors: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM crawl_state \
             WHERE last_crawled_at IS NULL \
                OR last_crawled_at < NOW() - INTERVAL '7 days'",
        )
        .fetch_one(&self.pool)
        .await
        .unwrap_or(0);

        tracing::info!(
            authors = total_authors,
            relays = self.confirmed_relays.len(),
            "negentropy_only: starting crawl"
        );

        let this = Arc::new(self);
        let mut shutdown_rx = shutdown.subscribe();
        let syncer = NegentropySyncer::new(
            this.repo.clone(),
            this.cache.clone(),
            this.pool.clone(),
        );

        let mut authors_processed: i64 = 0;
        let mut grand_total_discovered: usize = 0;
        let mut grand_total_inserted: usize = 0;
        let crawl_start = Instant::now();

        loop {
            // Check for shutdown
            if shutdown_rx.try_recv().is_ok() {
                tracing::info!("negentropy_only: shutdown signal received");
                break;
            }

            // Fetch next author: prefer never-crawled, then oldest last_crawled_at.
            // This resumes naturally after restarts — already-crawled authors are skipped.
            let author: Option<(String, i64, i16)> = sqlx::query_as(
                "SELECT pubkey, follower_count, priority_tier \
                 FROM crawl_state \
                 WHERE last_crawled_at IS NULL \
                    OR last_crawled_at < NOW() - INTERVAL '7 days' \
                 ORDER BY last_crawled_at ASC NULLS FIRST, \
                          priority_tier ASC, follower_count DESC \
                 LIMIT 1",
            )
            .fetch_optional(&this.pool)
            .await
            .unwrap_or(None);

            let (pubkey, follower_count, tier) = match author {
                Some(a) => a,
                None => {
                    tracing::info!(
                        authors_processed = authors_processed,
                        total_discovered = grand_total_discovered,
                        total_inserted = grand_total_inserted,
                        elapsed_secs = crawl_start.elapsed().as_secs(),
                        "negentropy_only: note pass complete"
                    );
                    break;
                }
            };

            authors_processed += 1;

            let short_pk = if pubkey.len() >= 12 {
                format!("{}..{}", &pubkey[..6], &pubkey[pubkey.len() - 6..])
            } else {
                pubkey.clone()
            };

            let mut author_discovered: usize = 0;
            let mut author_inserted: usize = 0;
            let author_start = Instant::now();

            // Sync against each confirmed relay
            for (relay_idx, relay_url) in this.confirmed_relays.iter().enumerate() {
                let relay_start = Instant::now();

                match syncer
                    .sync_authors(relay_url, &[1], &[pubkey.clone()])
                    .await
                {
                    Ok(stats) => {
                        author_discovered += stats.events_discovered;
                        author_inserted += stats.events_inserted;

                        tracing::info!(
                            author_num = authors_processed,
                            total_authors = total_authors,
                            pubkey = %short_pk,
                            tier = tier,
                            followers = follower_count,
                            relay_num = relay_idx + 1,
                            relay_count = this.confirmed_relays.len(),
                            relay = %relay_url,
                            discovered = stats.events_discovered,
                            inserted = stats.events_inserted,
                            duration_ms = relay_start.elapsed().as_millis() as u64,
                            "negentropy_only: author relay sync"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            author_num = authors_processed,
                            total_authors = total_authors,
                            pubkey = %short_pk,
                            relay = %relay_url,
                            error = %e,
                            "negentropy_only: sync failed"
                        );
                    }
                }

                // Brief delay between relays to be polite
                sleep(Duration::from_millis(100)).await;
            }

            // Update crawl state
            let oldest: Option<i64> = sqlx::query_scalar(
                "SELECT MIN(created_at) FROM events WHERE pubkey = $1 AND kind = 1",
            )
            .bind(&pubkey)
            .fetch_one(&this.pool)
            .await
            .unwrap_or(None);

            let newest: Option<i64> = sqlx::query_scalar(
                "SELECT MAX(created_at) FROM events WHERE pubkey = $1 AND kind = 1",
            )
            .bind(&pubkey)
            .fetch_one(&this.pool)
            .await
            .unwrap_or(None);

            let _ = this
                .queue
                .mark_crawled(&pubkey, author_inserted as i64, oldest, newest)
                .await;

            grand_total_discovered += author_discovered;
            grand_total_inserted += author_inserted;

            // Log author completion
            tracing::info!(
                author_num = authors_processed,
                total_authors = total_authors,
                pubkey = %short_pk,
                tier = tier,
                followers = follower_count,
                discovered = author_discovered,
                inserted = author_inserted,
                duration_ms = author_start.elapsed().as_millis() as u64,
                "negentropy_only: author complete"
            );

            // Progress summary every 50 authors
            if authors_processed % 50 == 0 {
                let elapsed = crawl_start.elapsed().as_secs();
                let rate = if elapsed > 0 {
                    authors_processed as f64 / elapsed as f64
                } else {
                    0.0
                };
                let remaining = if rate > 0.0 {
                    ((total_authors - authors_processed) as f64 / rate) as u64
                } else {
                    0
                };

                tracing::info!(
                    authors_processed = authors_processed,
                    total_authors = total_authors,
                    total_discovered = grand_total_discovered,
                    total_inserted = grand_total_inserted,
                    elapsed_secs = elapsed,
                    remaining_secs = remaining,
                    authors_per_sec = format!("{:.2}", rate),
                    "negentropy_only: progress"
                );
            }
        }

        // Final summary for note pass
        tracing::info!(
            authors_processed = authors_processed,
            total_authors = total_authors,
            total_discovered = grand_total_discovered,
            total_inserted = grand_total_inserted,
            elapsed_secs = crawl_start.elapsed().as_secs(),
            "negentropy_only: note pass finished"
        );

        // Phase 2: Zap crawl pass
        if shutdown_rx.try_recv().is_ok() {
            return;
        }
        Self::run_zap_pass(&this, &mut shutdown_rx).await;
    }

    // -----------------------------------------------------------------------
    // Zap crawl pass
    // -----------------------------------------------------------------------

    /// Discover top relays from NIP-65 data and fetch zap receipts for all authors.
    /// Uses traditional REQ with `#p` tag filter (not negentropy) so it works on
    /// all relays. Authors are batched in groups of 50 per relay connection.
    async fn run_zap_pass(
        this: &Arc<Self>,
        shutdown_rx: &mut broadcast::Receiver<()>,
    ) {
        // Discover top relays from NIP-65 data
        let zap_relays = match this.relay_router.get_top_relays(50).await {
            Ok(relays) => {
                let urls: Vec<String> = relays.into_iter().map(|(url, _)| url).collect();
                tracing::info!(
                    relay_count = urls.len(),
                    "zap_pass: discovered top relays from NIP-65"
                );
                urls
            }
            Err(e) => {
                tracing::warn!(error = %e, "zap_pass: failed to discover relays, using pinned");
                this.pinned_relays.clone()
            }
        };

        if zap_relays.is_empty() {
            tracing::warn!("zap_pass: no relays available, skipping");
            return;
        }

        // Count authors needing zap crawl (never crawled or older than 7 days)
        let total_to_crawl: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM crawl_state \
             WHERE zaps_crawled_at IS NULL \
                OR zaps_crawled_at < NOW() - INTERVAL '7 days'",
        )
        .fetch_one(&this.pool)
        .await
        .unwrap_or(0);

        if total_to_crawl == 0 {
            tracing::info!("zap_pass: all authors have recent zap data, skipping");
            return;
        }

        tracing::info!(
            authors = total_to_crawl,
            relays = zap_relays.len(),
            "zap_pass: starting zap crawl"
        );

        let zap_start = Instant::now();
        let mut authors_done: i64 = 0;
        let mut grand_zaps_inserted: u64 = 0;
        let mut offset: i64 = 0;
        let batch_size: i64 = 50;

        loop {
            if shutdown_rx.try_recv().is_ok() {
                tracing::info!("zap_pass: shutdown signal received");
                break;
            }

            // Fetch next batch of authors needing zap crawl
            let batch: Vec<String> = sqlx::query_scalar(
                "SELECT pubkey FROM crawl_state \
                 WHERE zaps_crawled_at IS NULL \
                    OR zaps_crawled_at < NOW() - INTERVAL '7 days' \
                 ORDER BY priority_tier ASC, follower_count DESC \
                 OFFSET $1 LIMIT $2",
            )
            .bind(offset)
            .bind(batch_size)
            .fetch_all(&this.pool)
            .await
            .unwrap_or_default();

            if batch.is_empty() {
                break;
            }

            let batch_start = Instant::now();
            let mut batch_inserted: u64 = 0;

            // Query each relay for zaps targeting this batch of authors
            for relay_url in &zap_relays {
                match Self::fetch_zaps_from_relay(
                    &this.repo,
                    &this.cache,
                    relay_url,
                    &batch,
                )
                .await
                {
                    Ok(inserted) => {
                        batch_inserted += inserted;
                    }
                    Err(e) => {
                        tracing::debug!(
                            relay = %relay_url,
                            error = %e,
                            "zap_pass: relay fetch failed"
                        );
                    }
                }

                // Brief delay between relays
                sleep(Duration::from_millis(100)).await;
            }

            // Mark batch as zap-crawled
            let _ = this.queue.mark_zaps_crawled(&batch).await;

            authors_done += batch.len() as i64;
            grand_zaps_inserted += batch_inserted;
            offset += batch_size;

            tracing::info!(
                authors_done = authors_done,
                total = total_to_crawl,
                batch_inserted = batch_inserted,
                total_inserted = grand_zaps_inserted,
                duration_ms = batch_start.elapsed().as_millis() as u64,
                "zap_pass: batch complete"
            );
        }

        tracing::info!(
            authors_done = authors_done,
            total_to_crawl = total_to_crawl,
            zaps_inserted = grand_zaps_inserted,
            elapsed_secs = zap_start.elapsed().as_secs(),
            "zap_pass: finished"
        );
    }

    /// Fetch kind-9735 zap receipts from a relay for a batch of authors using `#p` tag filter.
    async fn fetch_zaps_from_relay(
        repo: &EventRepository,
        cache: &StatsCache,
        relay_url: &str,
        pubkeys: &[String],
    ) -> Result<u64, String> {
        if pubkeys.is_empty() {
            return Ok(0);
        }

        let connect_timeout = Duration::from_secs(10);
        let msg_timeout = Duration::from_secs(30);

        let (ws_stream, _) = timeout(connect_timeout, tokio_tungstenite::connect_async(relay_url))
            .await
            .map_err(|_| format!("connect timeout: {relay_url}"))?
            .map_err(|e| format!("connect failed to {relay_url}: {e}"))?;

        let (mut write, mut read) = ws_stream.split();
        let mut total_inserted = 0u64;

        // Send a single REQ with all pubkeys (up to 50)
        let filter = serde_json::json!({
            "kinds": [9735],
            "#p": pubkeys,
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
                                    match repo.insert_event(&event, relay_url).await {
                                        Ok(true) => {
                                            total_inserted += 1;
                                            cache
                                                .on_event_ingested(&event.pubkey, event.kind)
                                                .await;
                                        }
                                        Ok(false) => {}
                                        Err(e) => {
                                            tracing::debug!(
                                                error = %e,
                                                "zap_pass: insert failed"
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

        Ok(total_inserted)
    }
}
