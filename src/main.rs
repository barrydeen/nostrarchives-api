mod api;
mod cache;
mod config;
mod db;
mod error;
mod relay;

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
    tracing::info!(
        listen = %cfg.listen_addr,
        relays = cfg.relay_urls.len(),
        "starting nostr-api"
    );

    // Database
    let pool = db::init_pool(&cfg.database_url)
        .await
        .expect("failed to connect to database");
    tracing::info!("database connected, migrations applied");

    let repo = db::repository::EventRepository::new(pool);

    // Redis
    let redis_client =
        redis::Client::open(cfg.redis_url.as_str()).expect("invalid redis url");
    // Verify connectivity
    redis_client
        .get_multiplexed_async_connection()
        .await
        .expect("failed to connect to redis");
    tracing::info!("redis connected");

    let stats_cache = cache::StatsCache::new(redis_client, repo.clone());

    // Shutdown signal
    let (shutdown_tx, _) = broadcast::channel::<()>(1);

    // Start relay ingestion
    let ingester = relay::ingester::RelayIngester::new(
        cfg.relay_urls.clone(),
        repo.clone(),
        stats_cache.clone(),
        cfg.ingestion_since,
    );
    ingester.run(shutdown_tx.clone()).await;

    // HTTP API
    let state = api::AppState {
        repo,
        cache: stats_cache,
    };
    let app = api::router(state);
    let addr: SocketAddr = cfg.listen_addr.parse().expect("invalid listen address");

    tracing::info!(addr = %addr, "api server listening");

    let listener = tokio::net::TcpListener::bind(addr).await.expect("failed to bind");

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
