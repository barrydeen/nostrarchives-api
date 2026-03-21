use std::sync::Arc;
use std::time::Instant;

use sqlx::PgPool;
use tokio::sync::broadcast;
use tokio::time::{sleep, Duration};

use crate::cache::StatsCache;
use crate::db::repository::EventRepository;

use super::negentropy::NegentropySyncer;
use super::queue::CrawlQueue;
use super::relay_caps;

/// Simple negentropy-only crawler: iterates through every author in crawl_state
/// and does a negentropy sync of their kind-1 notes against a pinned set of relays.
pub struct NegentropyOnlyCrawler {
    repo: EventRepository,
    cache: StatsCache,
    pool: PgPool,
    queue: CrawlQueue,
    pinned_relays: Vec<String>,
    confirmed_relays: Vec<String>,
}

impl NegentropyOnlyCrawler {
    pub fn new(
        repo: EventRepository,
        cache: StatsCache,
        pool: PgPool,
        queue: CrawlQueue,
        pinned_relays: Vec<String>,
    ) -> Self {
        Self {
            repo,
            cache,
            pool,
            queue,
            pinned_relays,
            confirmed_relays: Vec::new(),
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

        // Count total authors
        let total_authors: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM crawl_state")
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

        let mut offset: i64 = 0;

        loop {
            // Check for shutdown
            if shutdown_rx.try_recv().is_ok() {
                tracing::info!("negentropy_only: shutdown signal received");
                break;
            }

            // Fetch next author
            let author: Option<(String, i64, i16)> = sqlx::query_as(
                "SELECT pubkey, follower_count, priority_tier \
                 FROM crawl_state \
                 ORDER BY priority_tier ASC, follower_count DESC \
                 OFFSET $1 LIMIT 1",
            )
            .bind(offset)
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
                        "negentropy_only: full pass complete"
                    );
                    break;
                }
            };

            offset += 1;
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

        // Final summary
        tracing::info!(
            authors_processed = authors_processed,
            total_authors = total_authors,
            total_discovered = grand_total_discovered,
            total_inserted = grand_total_inserted,
            elapsed_secs = crawl_start.elapsed().as_secs(),
            "negentropy_only: crawl finished"
        );
    }
}
