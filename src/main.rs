mod api;
mod auth;
mod block_cache;
mod cache;
mod config;
mod crawler;
mod db;
mod error;
mod follower_cache;
mod indexer;
mod live_metrics;
mod nip19;
mod ratelimit;
mod relay;
mod social;
mod profile_search_cache;
mod wot_cache;
mod scheduler;
mod ws;

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::signal;
use tokio::sync::broadcast;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nostr_api=info,tower_http=info".into()),
        )
        .init();

    let cfg = config::Config::from_env();
    let mut relay_urls = cfg.relay_urls.clone();

    if cfg.relay_discovery_enabled && !cfg.relay_indexers.is_empty() {
        let discovery =
            relay::discovery::discover_relays(&cfg.relay_indexers, cfg.relay_target_count).await;

        if !discovery.relays.is_empty() {
            let mut dedup = HashSet::new();
            let mut combined = Vec::new();

            for url in &discovery.relays {
                if dedup.insert(url.clone()) {
                    combined.push(url.clone());
                }
            }

            for url in &cfg.relay_urls {
                if dedup.insert(url.clone()) {
                    combined.push(url.clone());
                }
            }

            tracing::info!(
                discovered = discovery.relays.len(),
                relay_lists = discovery.relay_lists_processed,
                candidates = discovery.candidates_seen,
                active_relays = combined.len(),
                "relay discovery completed"
            );

            relay_urls = combined;
        } else {
            tracing::warn!(
                indexers = cfg.relay_indexers.len(),
                "relay discovery produced no relays; using configured RELAY_URLS"
            );
        }
    } else if !cfg.relay_discovery_enabled {
        tracing::info!("relay discovery disabled; using configured RELAY_URLS");
    }

    tracing::info!(
        listen = %cfg.listen_addr,
        relays = relay_urls.len(),
        "starting nostr-api"
    );

    // Database
    let pool = db::init_pool(&cfg.database_url)
        .await
        .expect("failed to connect to database");
    tracing::info!("database connected, migrations applied");

    // Follower cache for high-performance threshold checking (legacy, kept for stats endpoint)
    let follower_cache = follower_cache::FollowerCache::new(
        pool.clone(),
        cfg.min_follower_threshold,
        cfg.follower_cache_refresh_secs,
    );

    // Initialize the cache on startup
    if let Err(e) = follower_cache.initialize().await {
        tracing::warn!(error = %e, "Failed to initialize follower cache, continuing anyway");
    }

    // Web of Trust cache: two-level follower quality check
    let wot_cache = wot_cache::WotCache::new(
        pool.clone(),
        cfg.wot_threshold,
        cfg.wot_refresh_secs,
    );

    if let Err(e) = wot_cache.initialize().await {
        tracing::warn!(error = %e, "Failed to initialize WoT cache, continuing anyway");
    }

    // Block cache: in-memory moderation lists
    let block_cache = block_cache::BlockCache::new(pool.clone());
    if let Err(e) = block_cache.initialize().await {
        tracing::warn!(error = %e, "Failed to initialize block cache, continuing anyway");
    }

    let repo = db::repository::EventRepository::new(pool.clone(), follower_cache, wot_cache, block_cache.clone());

    // Backfill zero-amount zaps with bolt11 parsing (one-time startup task)
    match repo.backfill_zero_amount_zaps().await {
        Ok(count) => tracing::info!(updated_zaps = count, "zap amount backfill completed"),
        Err(e) => tracing::warn!(error = %e, "zap amount backfill failed"),
    }

    if cfg.social_graph_bootstrap {
        // Skip bootstrap if the social graph is already populated (e.g. from a previous run).
        let existing_follows: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM follow_lists")
            .fetch_one(&pool)
            .await
            .unwrap_or((0,));

        if existing_follows.0 >= 100 {
            tracing::info!(
                follow_lists = existing_follows.0,
                "social graph already populated, skipping bootstrap"
            );
        } else {
            // Bootstrap blocks startup so the social graph is built before
            // the crawler tries to seed its queue from WoT scores.
            social::builder::bootstrap_social_graph(repo.clone(), relay_urls.clone()).await;

            // Refresh WoT cache now that we have follow data
            tracing::info!("refreshing WoT cache after social graph bootstrap");
            if let Err(e) = repo.wot_cache.initialize().await {
                tracing::warn!(error = %e, "WoT cache refresh after bootstrap failed");
            }

            // Refresh profile_search materialized view so client leaderboard/search works
            tracing::info!("refreshing profile_search after social graph bootstrap");
            match repo.refresh_profile_search().await {
                Ok(()) => tracing::info!("profile_search refreshed after bootstrap"),
                Err(e) => tracing::warn!(error = %e, "profile_search refresh after bootstrap failed"),
            }
        }
    } else {
        tracing::info!("social graph bootstrap disabled");
    }

    // Redis
    let redis_client = redis::Client::open(cfg.redis_url.as_str()).expect("invalid redis url");
    // Verify connectivity
    redis_client
        .get_multiplexed_async_connection()
        .await
        .expect("failed to connect to redis");
    tracing::info!("redis connected");

    let live_tracker = Arc::new(live_metrics::LiveMetricsTracker::new(redis_client.clone()));

    // Background: clean up stale active users from Redis sorted set every 60s
    {
        let cleanup_tracker = live_tracker.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                cleanup_tracker.cleanup_active_users().await;
            }
        });
    }

    let mut stats_cache = cache::StatsCache::new(redis_client, repo.clone());
    stats_cache.set_live_tracker(live_tracker.clone());

    // Start the background purge worker (processes blocked pubkey data deletion)
    block_cache.spawn_purge_worker(stats_cache.clone());

    // Shutdown signal
    let (shutdown_tx, _) = broadcast::channel::<()>(1);

    // Start metadata resolver (fetches kind-0 for discovered pubkeys).
    // Use the configured relay_urls (reliable indexers), not the full discovered list.
    let metadata_resolver =
        relay::metadata::MetadataResolver::new(repo.clone(), cfg.relay_urls.clone());
    let metadata_tx = metadata_resolver.start(shutdown_tx.clone());

    // Start relay ingestion (with metadata resolver attached)
    let ingester = relay::ingester::RelayIngester::new(
        relay_urls.clone(),
        repo.clone(),
        stats_cache.clone(),
        cfg.ingestion_since,
    )
    .with_metadata_sender(metadata_tx);
    ingester.run(shutdown_tx.clone()).await;

    // Start intelligent crawler (historical note backfill)
    let crawl_queue = if cfg.crawler_enabled {
        let queue = crawler::queue::CrawlQueue::new(repo.pool());

        match cfg.crawl_mode.as_str() {
            "negentropy_only" => {
                let relay_router = crawler::relay_router::RelayRouter::new(repo.pool());
                let mut neg_crawler = crawler::negentropy_only::NegentropyOnlyCrawler::new(
                    repo.clone(),
                    stats_cache.clone(),
                    repo.pool(),
                    queue.clone(),
                    cfg.negentropy_pinned_relays.clone(),
                    relay_router,
                );
                let neg_shutdown = shutdown_tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                    neg_crawler.validate_relays().await;
                    neg_crawler.run(neg_shutdown).await;
                });
                tracing::info!(
                    relays = cfg.negentropy_pinned_relays.len(),
                    "negentropy-only crawler enabled"
                );
            }
            _ => {
                // Start hybrid crawler (negentropy + relay-list-aware) if enabled
                if cfg.negentropy_enabled || cfg.crawler_use_relay_lists {
                    let hybrid_config = crawler::orchestrator::HybridCrawlerConfig {
                        negentropy_enabled: cfg.negentropy_enabled,
                        negentropy_sync_interval_secs: cfg.negentropy_sync_interval_secs,
                        negentropy_max_relays: cfg.negentropy_max_relays,
                        use_relay_lists: cfg.crawler_use_relay_lists,
                        max_relay_pool_size: cfg.crawler_max_relay_pool_size,
                        legacy_batch_size: cfg.crawler_batch_size,
                        legacy_request_delay_ms: cfg.crawler_request_delay_ms,
                        legacy_poll_interval_secs: cfg.crawler_poll_interval_secs,
                        legacy_events_per_author: cfg.crawler_events_per_author,
                        fallback_relay_urls: cfg.relay_urls.clone(),
                        primary_negentropy_relay_urls: cfg.negentropy_relay_urls.clone(),
                        dry_run: cfg.crawler_dry_run,
                    };
                    let router = crawler::relay_router::RelayRouter::new(repo.pool());
                    let hybrid = crawler::orchestrator::HybridCrawler::new(
                        hybrid_config,
                        repo.clone(),
                        stats_cache.clone(),
                        repo.pool(),
                        queue.clone(),
                        router,
                    );
                    let hybrid_shutdown = shutdown_tx.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                        hybrid.run(hybrid_shutdown).await;
                    });
                    tracing::info!("hybrid crawler enabled (negentropy={}, relay_lists={})",
                        cfg.negentropy_enabled, cfg.crawler_use_relay_lists);
                } else {
                    // Fall back to legacy crawler
                    let crawler_config = crawler::worker::CrawlerConfig {
                        relay_urls: cfg.relay_urls.clone(),
                        batch_size: cfg.crawler_batch_size,
                        events_per_author: cfg.crawler_events_per_author,
                        request_delay_ms: cfg.crawler_request_delay_ms,
                        poll_interval_secs: cfg.crawler_poll_interval_secs,
                        sync_interval_secs: cfg.crawler_sync_interval_secs,
                        max_concurrency: cfg.crawler_max_concurrency,
                    };
                    let crawler_worker = crawler::worker::Crawler::new(
                        crawler_config,
                        queue.clone(),
                        repo.clone(),
                        stats_cache.clone(),
                    );
                    let crawler_shutdown = shutdown_tx.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                        crawler_worker.run(crawler_shutdown).await;
                    });
                    tracing::info!("legacy crawler enabled");
                }
            }
        }
        Some(queue)
    } else {
        tracing::info!("crawler disabled");
        None
    };

    // Background: refresh profile_search materialized view once per day.
    let refresh_repo = repo.clone();
    tokio::spawn(async move {
        // Initial delay: let the service stabilize before first refresh.
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        loop {
            match refresh_repo.refresh_profile_search().await {
                Ok(()) => tracing::info!("refreshed profile_search materialized view"),
                Err(e) => tracing::warn!("failed to refresh profile_search: {e}"),
            }
            tokio::time::sleep(std::time::Duration::from_secs(86400)).await;
        }
    });

    // Background: refresh analytics materialized views every 30 minutes,
    // then flush the corresponding Redis caches so stale data is never served.
    let analytics_mv_repo = repo.clone();
    let analytics_mv_cache = stats_cache.clone();
    tokio::spawn(async move {
        // Initial delay: let migrations and profile_search finish first.
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        loop {
            match analytics_mv_repo.refresh_analytics_views().await {
                Ok(()) => {
                    tracing::info!("refreshed analytics materialized views");
                    for prefix in &[
                        "analytics:top_posters",
                        "analytics:most_liked",
                        "analytics:most_shared",
                        "clients:leaderboard",
                        "clients:users",
                        "relays:leaderboard",
                    ] {
                        analytics_mv_cache.delete_by_prefix(prefix).await;
                    }
                }
                Err(e) => tracing::warn!("failed to refresh analytics views: {e}"),
            }
            tokio::time::sleep(std::time::Duration::from_secs(1800)).await;
        }
    });

    // Background: compute daily analytics.
    // On startup: backfill last 30 days. Then loop: sleep until next midnight UTC, compute yesterday.
    let analytics_repo = repo.clone();
    let analytics_cache = stats_cache.clone();
    tokio::spawn(async move {
        // Backfill on startup
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        match analytics_repo.backfill_daily_analytics(30).await {
            Ok(n) => tracing::info!(days_computed = n, "daily analytics backfill complete"),
            Err(e) => tracing::warn!("daily analytics backfill failed: {e}"),
        }

        // Daily loop: sleep until next midnight UTC, then compute yesterday
        loop {
            let now = chrono::Utc::now();
            let tomorrow_midnight = (now.date_naive() + chrono::Duration::days(1))
                .and_hms_opt(0, 0, 0)
                .unwrap()
                .and_utc();
            let sleep_duration = (tomorrow_midnight - now)
                .to_std()
                .unwrap_or(std::time::Duration::from_secs(3600));
            tokio::time::sleep(sleep_duration).await;

            let yesterday = chrono::Utc::now().date_naive() - chrono::Duration::days(1);
            match analytics_repo.compute_daily_analytics(yesterday).await {
                Ok(()) => {
                    tracing::info!(date = %yesterday, "daily analytics computed");
                    // Invalidate cached responses so users see fresh data immediately
                    for days in [7, 30, 365] {
                        analytics_cache.delete_json(&format!("analytics:daily:{days}")).await;
                    }
                }
                Err(e) => tracing::warn!(date = %yesterday, "daily analytics computation failed: {e}"),
            }
        }
    });

    // Background: pre-compute slow queries every 5 minutes.
    // Keeps Redis cache warm so neither HTTP handlers nor WS feeds ever compute on-demand.
    let home_repo = repo.clone();
    let home_cache = stats_cache.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        loop {
            // ── Homepage trending notes (limit 10, 20) ──────────────────
            for limit in [10i64, 20] {
                match home_repo.trending_notes(limit, 0).await {
                    Ok(notes) => {
                        let response = serde_json::json!({ "notes": notes });
                        if let Ok(json_str) = serde_json::to_string(&response) {
                            home_cache.set_json(
                                &format!("home:trending:{limit}:0"),
                                &json_str,
                                600,
                            ).await;
                        }
                        tracing::info!(limit, "pre-computed trending notes");
                    }
                    Err(e) => tracing::warn!(error = %e, "failed to pre-compute trending notes"),
                }
            }

            // ── Homepage trending users / hashtags ──────────────────────
            match home_repo.trending_users(12, 0).await {
                Ok(users) => {
                    let response = serde_json::json!({ "users": users });
                    if let Ok(json_str) = serde_json::to_string(&response) {
                        home_cache.set_json("home:trending_users:12:0", &json_str, 600).await;
                    }
                    tracing::info!("pre-computed trending users");
                }
                Err(e) => tracing::warn!(error = %e, "failed to pre-compute trending users"),
            }

            match home_repo.trending_hashtags(20, 0).await {
                Ok(hashtags) => {
                    let response = serde_json::json!({ "hashtags": hashtags });
                    if let Ok(json_str) = serde_json::to_string(&response) {
                        home_cache.set_json("home:trending_hashtags:20:0", &json_str, 600).await;
                    }
                    tracing::info!("pre-computed trending hashtags");
                }
                Err(e) => tracing::warn!(error = %e, "failed to pre-compute trending hashtags"),
            }

            // ── Trending page + WS feeds (top notes by metric/range) ────
            // Pre-compute limit=20 (HTTP trending page) and limit=100 (WS feeds).
            // Uses the same cache key format as set_trending: "trending:{metric}:{range}:{limit}:0"
            let metrics = ["reactions", "replies", "reposts", "zaps"];
            let ranges_with_since: Vec<(&str, Option<i64>)> = {
                let now = chrono::Utc::now().timestamp();
                vec![
                    ("today", Some(now - 86_400)),
                    ("7d", Some(now - 7 * 86_400)),
                    ("30d", Some(now - 30 * 86_400)),
                    ("1y", Some(now - 365 * 86_400)),
                    ("all", None),
                ]
            };

            for metric in &metrics {
                let ref_type = match *metric {
                    "reactions" => "reaction",
                    "replies" => "reply",
                    "reposts" => "repost",
                    "zaps" => "zap",
                    _ => continue,
                };

                for (range, since) in &ranges_with_since {
                    for limit in [20i64, 100] {
                        match home_repo.top_notes_unified(ref_type, *since, limit, 0).await {
                            Ok((ranked, profile_rows)) => {
                                let profiles: std::collections::HashMap<String, serde_json::Value> = profile_rows
                                    .into_iter()
                                    .filter_map(|row| {
                                        serde_json::from_str::<serde_json::Value>(&row.content).ok().map(|v| {
                                            let entry = serde_json::json!({
                                                "name": v.get("name").and_then(|n| n.as_str()).filter(|s| !s.trim().is_empty()),
                                                "display_name": v.get("display_name").or_else(|| v.get("displayName")).and_then(|n| n.as_str()).filter(|s| !s.trim().is_empty()),
                                                "picture": v.get("picture").or_else(|| v.get("image")).and_then(|n| n.as_str()).filter(|s| !s.trim().is_empty()),
                                                "nip05": v.get("nip05").and_then(|n| n.as_str()).filter(|s| !s.trim().is_empty()),
                                            });
                                            (row.pubkey.clone(), entry)
                                        })
                                    })
                                    .collect();

                                let notes: Vec<serde_json::Value> = ranked
                                    .into_iter()
                                    .map(|entry| serde_json::json!({
                                        "count": entry.count,
                                        "total_sats": entry.total_sats,
                                        "reactions": entry.reactions,
                                        "replies": entry.replies,
                                        "reposts": entry.reposts,
                                        "zap_sats": entry.zap_sats,
                                        "event": entry.event,
                                    }))
                                    .collect();

                                let response = serde_json::json!({
                                    "metric": metric,
                                    "range": range,
                                    "notes": notes,
                                    "profiles": profiles,
                                });
                                if let Ok(json_str) = serde_json::to_string(&response) {
                                    home_cache.set_trending(metric, range, limit, 0, &json_str).await;
                                }
                            }
                            Err(e) => tracing::warn!(
                                metric, range, limit,
                                error = %e,
                                "failed to pre-compute top notes"
                            ),
                        }
                    }
                }
                tracing::info!(metric, "pre-computed top notes (all ranges)");
            }

            tokio::time::sleep(std::time::Duration::from_secs(300)).await;
        }
    });

    // Initialize on-demand relay fetcher
    let relay_router = crawler::relay_router::RelayRouter::new(pool.clone());
    let fetcher = Arc::new(relay::fetcher::RelayFetcher::new(
        repo.clone(),
        relay_router,
        cfg.relay_urls.clone(),
        stats_cache.clone(),
        cfg.ondemand_fetch_timeout_ms,
        cfg.ondemand_fetch_max_relays,
        cfg.ondemand_fetch_enabled,
    ));

    // Background: drain the missing_events queue.
    // When the ingester receives a reaction/repost/zap whose target note isn't in the DB,
    // it queues the missing event ID. This task fetches them via RelayFetcher and then
    // reapplies engagement counters so linkage is correct.
    {
        let missing_repo = repo.clone();
        let missing_fetcher = Arc::clone(&fetcher);
        tokio::spawn(async move {
            // Initial delay — let the service stabilize before hitting relays.
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(120));
            loop {
                interval.tick().await;
                let events = match missing_repo.take_missing_events(50).await {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!(error = %e, "missing events: failed to take batch");
                        continue;
                    }
                };
                if events.is_empty() {
                    continue;
                }
                tracing::info!(count = events.len(), "missing events: processing batch");
                for missing in events {
                    let hints = missing
                        .relay_hint
                        .as_deref()
                        .filter(|h| !h.is_empty())
                        .map(|h| vec![h.to_string()])
                        .unwrap_or_default();
                    match missing_fetcher
                        .fetch_event_by_id(&missing.event_id, &hints)
                        .await
                    {
                        Ok(Some(_)) => {
                            if let Err(e) = missing_repo
                                .reapply_counters_for_event(&missing.event_id)
                                .await
                            {
                                tracing::debug!(error = %e, "missing events: counter reapply failed");
                            }
                            if let Err(e) = missing_repo
                                .mark_missing_event_fetched(&missing.event_id)
                                .await
                            {
                                tracing::debug!(error = %e, "missing events: mark fetched failed");
                            }
                            tracing::info!(event_id = %missing.event_id, "missing events: fetched and counters applied");
                        }
                        Ok(None) => {
                            let _ = missing_repo
                                .mark_missing_event_attempted(&missing.event_id)
                                .await;
                        }
                        Err(e) => {
                            tracing::debug!(error = %e, event_id = %missing.event_id, "missing events: fetch error");
                            let _ = missing_repo
                                .mark_missing_event_attempted(&missing.event_id)
                                .await;
                        }
                    }
                    // Small pause between fetches to be polite to relays
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
        });
    }

    // In-memory profile search cache (zero-DB-hit searches)
    let profile_search_cache = profile_search_cache::ProfileSearchCache::new(
        pool.clone(),
        cfg.profile_search_cache_refresh_secs,
    );
    if let Err(e) = profile_search_cache.initialize().await {
        tracing::warn!(error = %e, "Failed to initialize profile search cache, continuing anyway");
    }
    // Spawn background refresh (same interval as config, re-loads from profile_search MV)
    profile_search_cache
        .clone()
        .spawn_refresh_loop(std::time::Duration::from_secs(
            cfg.profile_search_cache_refresh_secs,
        ));

    // HTTP API
    let state = api::AppState {
        repo,
        cache: stats_cache,
        crawl_queue,
        fetcher,
        profile_search_cache,
        live_tracker: Some(live_tracker),
        block_cache,
        admin_pubkey: cfg.admin_pubkey.clone(),
        replay_guard: auth::ReplayGuard::new(),
    };

    // WebSocket relay (NIP-50 search endpoint)
    let ws_addr: SocketAddr = cfg
        .ws_listen_addr
        .parse()
        .expect("invalid ws listen address");
    let ws_shutdown_rx = shutdown_tx.subscribe();
    tokio::spawn(ws::serve(state.clone(), ws_addr, ws_shutdown_rx));

    // Scheduler relay (future-dated event scheduling)
    if cfg.scheduler_enabled {
        let scheduler_addr: SocketAddr = cfg
            .scheduler_ws_listen_addr
            .parse()
            .expect("invalid scheduler ws listen address");

        let scheduler_relay_router = crawler::relay_router::RelayRouter::new(pool.clone());
        let scheduler_state = scheduler::SchedulerState {
            pool: pool.clone(),
            relay_router: scheduler_relay_router,
            top_relays: Arc::new(tokio::sync::RwLock::new(Vec::new())),
        };

        // Spawn top relays cache refresher
        tokio::spawn(scheduler::refresh_top_relays_loop(scheduler_state.clone()));

        // Spawn the background publisher (checks every 60s for due events)
        let publisher_shutdown_rx = shutdown_tx.subscribe();
        tokio::spawn(scheduler::run_publisher(
            scheduler_state.clone(),
            publisher_shutdown_rx,
        ));

        // Spawn the WebSocket listener
        let scheduler_shutdown_rx = shutdown_tx.subscribe();
        tokio::spawn(scheduler::serve(
            scheduler_state,
            scheduler_addr,
            scheduler_shutdown_rx,
        ));

        tracing::info!(addr = %scheduler_addr, "scheduler relay enabled");
    } else {
        tracing::info!("scheduler relay disabled");
    }

    // Indexer relay (restricted: kinds 0, 3, 10002 only)
    if cfg.indexer_enabled {
        let indexer_addr: SocketAddr = cfg
            .indexer_ws_listen_addr
            .parse()
            .expect("invalid indexer ws listen address");
        let indexer_state = indexer::IndexerState::new(state.repo.clone());
        let indexer_shutdown_rx = shutdown_tx.subscribe();
        tokio::spawn(indexer::serve(indexer_state, indexer_addr, indexer_shutdown_rx));
        tracing::info!(addr = %indexer_addr, "indexer relay enabled");
    } else {
        tracing::info!("indexer relay disabled");
    }

    // Background: hashtag feed refresh (kind-30015 events cached in Redis)
    if cfg.feeds_enabled {
        let feeds_repo = state.repo.clone();
        let feeds_cache = state.cache.clone();
        let feeds_secret = cfg.feeds_signing_secret;
        tokio::spawn(ws::refresh_hashtag_feeds(feeds_repo, feeds_cache, feeds_secret));
        tracing::info!("hashtag feeds background refresh enabled");
    } else {
        tracing::info!("hashtag feeds disabled");
    }

    let app = api::router(state).into_make_service_with_connect_info::<SocketAddr>();
    let addr: SocketAddr = cfg.listen_addr.parse().expect("invalid listen address");

    tracing::info!(addr = %addr, "api server listening");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown_tx))
        .await
        .expect("server error");
}

async fn shutdown_signal(shutdown_tx: broadcast::Sender<()>) {
    let ctrl_c = async {
        signal::ctrl_c().await.expect("failed to listen for ctrl+c");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to listen for SIGTERM")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }

    tracing::info!("shutdown signal received");
    let _ = shutdown_tx.send(());
}
