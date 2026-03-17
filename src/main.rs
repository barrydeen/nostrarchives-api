mod api;
mod cache;
mod config;
mod crawler;
mod db;
mod error;
mod nip19;
mod ratelimit;
mod relay;
mod social;
mod ws;

use std::collections::HashSet;
use std::net::SocketAddr;
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

    let repo = db::repository::EventRepository::new(pool, cfg.min_follower_threshold);

    if cfg.social_graph_bootstrap {
        let builder_repo = repo.clone();
        let builder_relays = relay_urls.clone();
        tokio::spawn(async move {
            social::builder::bootstrap_social_graph(builder_repo, builder_relays).await;
        });
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

    let stats_cache = cache::StatsCache::new(redis_client, repo.clone());

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
        Some(queue)
    } else {
        tracing::info!("crawler disabled");
        None
    };

    // Background: refresh profile_search materialized view every 5 minutes.
    let refresh_repo = repo.clone();
    tokio::spawn(async move {
        // Initial delay: let the service stabilize before first refresh.
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
        loop {
            interval.tick().await;
            match refresh_repo.refresh_profile_search().await {
                Ok(()) => tracing::info!("refreshed profile_search materialized view"),
                Err(e) => tracing::warn!("failed to refresh profile_search: {e}"),
            }
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

    // HTTP API
    let state = api::AppState {
        repo,
        cache: stats_cache,
        crawl_queue,
    };

    // WebSocket relay (NIP-50 search endpoint)
    let ws_addr: SocketAddr = cfg
        .ws_listen_addr
        .parse()
        .expect("invalid ws listen address");
    let ws_shutdown_rx = shutdown_tx.subscribe();
    tokio::spawn(ws::serve(state.clone(), ws_addr, ws_shutdown_rx));

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
