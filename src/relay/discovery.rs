use std::collections::{HashMap, HashSet};
use std::fmt;
use std::time::Duration;

use futures_util::{future, SinkExt, StreamExt};
use serde_json::Value;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use crate::db::models::NostrEvent;

const RELAY_LIST_KIND: i64 = 10002;
const MESSAGE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_EVENTS_PER_INDEXER: usize = 2000;

#[derive(Debug)]
struct DiscoveryError {
    message: String,
}

impl DiscoveryError {
    fn new(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
        }
    }
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for DiscoveryError {}

#[derive(Debug, Default)]
pub struct RelayDiscoverySummary {
    pub relays: Vec<String>,
    pub candidates_seen: usize,
    pub relay_lists_processed: usize,
}

struct IndexerResult {
    relays: Vec<String>,
    relay_list_events: usize,
}

/// Discover relays by querying indexer relays for kind-10002 relay lists (NIP-65).
pub async fn discover_relays(
    indexer_urls: &[String],
    target_count: usize,
) -> RelayDiscoverySummary {
    if indexer_urls.is_empty() {
        return RelayDiscoverySummary::default();
    }

    let discovery_tasks = indexer_urls
        .iter()
        .cloned()
        .map(fetch_indexer_relays)
        .collect::<Vec<_>>();

    let results = future::join_all(discovery_tasks).await;

    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut relay_lists_processed = 0usize;

    for (indexer_url, result) in indexer_urls.iter().zip(results.into_iter()) {
        match result {
            Ok(indexer_result) => {
                relay_lists_processed += indexer_result.relay_list_events;
                for relay in indexer_result.relays {
                    *counts.entry(relay).or_insert(0usize) += 1;
                }
            }
            Err(err) => {
                tracing::warn!(indexer = indexer_url.as_str(), error = %err, "relay discovery failed");
            }
        }
    }

    let mut ordered: Vec<(String, usize)> = counts.into_iter().collect();
    ordered.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let candidates_seen = ordered.len();

    let relays: Vec<String> = ordered
        .into_iter()
        .map(|(url, _)| url)
        .take(target_count)
        .collect();

    RelayDiscoverySummary {
        candidates_seen,
        relay_lists_processed,
        relays,
    }
}

async fn fetch_indexer_relays(indexer_url: String) -> Result<IndexerResult, DiscoveryError> {
    let normalized_url = normalize_ws_url(&indexer_url);
    let (ws_stream, _) = tokio_tungstenite::connect_async(&normalized_url)
        .await
        .map_err(|e| DiscoveryError::new(format!("connect failed: {e}")))?;
    let (mut write, mut read) = ws_stream.split();

    let sub_id = format!("relay-discovery-{}", Uuid::new_v4().simple());
    let req = serde_json::json!([
        "REQ",
        &sub_id,
        {
            "kinds": [RELAY_LIST_KIND],
            "limit": MAX_EVENTS_PER_INDEXER as i64,
        }
    ]);

    write
        .send(Message::Text(req.to_string().into()))
        .await
        .map_err(|e| DiscoveryError::new(format!("failed to send REQ: {e}")))?;

    let mut collected: Vec<String> = Vec::new();
    let mut events_seen = 0usize;

    loop {
        match timeout(MESSAGE_TIMEOUT, read.next()).await {
            Ok(Some(Ok(message))) => match message {
                Message::Text(payload) => {
                    if let Some(event) = parse_relay_list_event(&payload) {
                        events_seen += 1;
                        collected.extend(event_relay_urls(&event));
                        if events_seen >= MAX_EVENTS_PER_INDEXER {
                            break;
                        }
                    }
                }
                Message::Ping(data) => {
                    let _ = write.send(Message::Pong(data)).await;
                }
                Message::Pong(_) => {}
                Message::Close(_) => break,
                Message::Binary(_) | Message::Frame(_) => {}
            },
            Ok(Some(Err(e))) => {
                return Err(DiscoveryError::new(format!("ws error: {e}")));
            }
            Ok(None) => break,
            Err(_) => {
                break; // timed out waiting for more messages
            }
        }
    }

    Ok(IndexerResult {
        relays: collected,
        relay_list_events: events_seen,
    })
}

fn parse_relay_list_event(text: &str) -> Option<NostrEvent> {
    let parsed: Value = serde_json::from_str(text).ok()?;
    let arr = parsed.as_array()?;

    if arr.len() < 3 {
        return None;
    }

    if arr.first().and_then(|v| v.as_str())? != "EVENT" {
        return None;
    }

    let event: NostrEvent = serde_json::from_value(arr[2].clone()).ok()?;
    if event.kind != RELAY_LIST_KIND {
        return None;
    }

    Some(event)
}

fn event_relay_urls(event: &NostrEvent) -> Vec<String> {
    let mut relays = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for tag in &event.tags {
        if tag.first().map(|v| v == "r").unwrap_or(false) {
            if let Some(raw_url) = tag.get(1) {
                if let Some(url) = normalize_relay_url(raw_url) {
                    if seen.insert(url.clone()) {
                        relays.push(url);
                    }
                }
            }
        }
    }

    relays
}

fn normalize_relay_url(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let normalized = if trimmed.starts_with("ws://") || trimmed.starts_with("wss://") {
        trimmed.to_string()
    } else if trimmed.starts_with("http://") {
        trimmed.replacen("http://", "ws://", 1)
    } else if trimmed.starts_with("https://") {
        trimmed.replacen("https://", "wss://", 1)
    } else {
        format!("wss://{}", trimmed.trim_start_matches('/'))
    };

    Some(normalized.trim_end_matches('/').to_string())
}

fn normalize_ws_url(raw: &str) -> String {
    normalize_relay_url(raw).unwrap_or_else(|| raw.trim().to_string())
}
