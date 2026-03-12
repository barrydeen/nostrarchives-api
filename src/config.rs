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

        let listen_addr = env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8000".into());

        let ingestion_since = env::var("INGESTION_SINCE")
            .ok()
            .and_then(|v| v.parse::<i64>().ok());

        Self {
            database_url,
            redis_url,
            relay_urls,
            listen_addr,
            ingestion_since,
            relay_indexers,
            relay_discovery_enabled,
            relay_target_count,
        }
    }
}
