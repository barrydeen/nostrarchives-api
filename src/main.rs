mod api;
mod cache;
mod config;
mod db;
mod error;
mod relay;
mod social;

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

    let repo = db::repository::EventRepository::new(pool);

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

    // Start metadata resolver (fetches kind-0 for discovered pubkeys)
    let metadata_resolver =
        relay::metadata::MetadataResolver::new(repo.clone(), relay_urls.clone());
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

    // HTTP API
    let state = api::AppState {
        repo,
        cache: stats_cache,
    };
    let app = api::router(state);
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
