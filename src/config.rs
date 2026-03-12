use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub redis_url: String,
    pub relay_urls: Vec<String>,
    pub listen_addr: String,
    pub ingestion_since: Option<i64>,
    pub relay_indexers: Vec<String>,
    pub relay_discovery_enabled: bool,
    pub relay_target_count: usize,
    pub social_graph_bootstrap: bool,
    pub ws_listen_addr: String,
    pub crawler_enabled: bool,
    pub crawler_batch_size: i64,
    pub crawler_events_per_author: i64,
    pub crawler_request_delay_ms: u64,
    pub crawler_poll_interval_secs: u64,
    pub crawler_sync_interval_secs: u64,
    pub crawler_max_concurrency: usize,
}

impl Config {
    pub fn from_env() -> Self {
        let database_url = env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://dev:dev@localhost:5432/nostr_api".into());

        let redis_url = env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());

        let relay_urls: Vec<String> = env::var("RELAY_URLS")
            .unwrap_or_else(|_| {
                [
                    "wss://relay.damus.io",
                    "wss://nos.lol",
                    "wss://relay.nostr.band",
                    "wss://relay.primal.net",
                    "wss://nostr.wine",
                ]
                .join(",")
            })
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let relay_indexers: Vec<String> = env::var("RELAY_INDEXERS")
            .unwrap_or_else(|_| {
                [
                    "wss://relay.damus.io",
                    "wss://relay.primal.net",
                    "wss://indexer.coracle.social",
                    "wss://relay.nos.social",
                ]
                .join(",")
            })
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let relay_discovery_enabled = env::var("ENABLE_RELAY_DISCOVERY")
            .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(true);

        let relay_target_count = env::var("RELAY_DISCOVERY_TARGET")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(25);

        let social_graph_bootstrap = env::var("ENABLE_SOCIAL_GRAPH_BOOTSTRAP")
            .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(true);

        let listen_addr = env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8000".into());

        let ws_listen_addr = env::var("WS_LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8001".into());

        let ingestion_since = env::var("INGESTION_SINCE")
            .ok()
            .and_then(|v| v.parse::<i64>().ok());

        let crawler_enabled = env::var("ENABLE_CRAWLER")
            .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(true);

        let crawler_batch_size = env::var("CRAWLER_BATCH_SIZE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10);

        let crawler_events_per_author = env::var("CRAWLER_EVENTS_PER_AUTHOR")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(500);

        let crawler_request_delay_ms = env::var("CRAWLER_REQUEST_DELAY_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(500);

        let crawler_poll_interval_secs = env::var("CRAWLER_POLL_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30);

        let crawler_sync_interval_secs = env::var("CRAWLER_SYNC_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3600);

        let crawler_max_concurrency = env::var("CRAWLER_MAX_CONCURRENCY")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3);

        Self {
            database_url,
            redis_url,
            relay_urls,
            listen_addr,
            ingestion_since,
            relay_indexers,
            relay_discovery_enabled,
            relay_target_count,
            social_graph_bootstrap,
            ws_listen_addr,
            crawler_enabled,
            crawler_batch_size,
            crawler_events_per_author,
            crawler_request_delay_ms,
            crawler_poll_interval_secs,
            crawler_sync_interval_secs,
            crawler_max_concurrency,
        }
    }
}
