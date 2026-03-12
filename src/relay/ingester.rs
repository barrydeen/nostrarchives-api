use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_tungstenite::tungstenite::Message;

use crate::cache::StatsCache;
use crate::db::models::NostrEvent;
use crate::db::repository::EventRepository;

/// Ingested event with source relay info.
struct IngestedEvent {
    event: NostrEvent,
    relay_url: String,
}

/// Kinds we ingest. Everything else is dropped.
///
/// - 0:     Profile metadata (NIP-01)
/// - 1:     Short text note (NIP-01)
/// - 6:     Repost (NIP-18)
/// - 7:     Reaction (NIP-25)
/// - 16:    Generic repost (NIP-18)
/// - 9735:  Zap receipt (NIP-57)
/// - 10000: Mute list (NIP-51)
/// - 10002: Relay list (NIP-65)
/// - 10003: Bookmarks (NIP-51)
/// - 10063: Media server list (Blossom)
/// - 30000: Follow sets (NIP-51)
/// - 30001: Generic lists (NIP-51)
/// - 30002: Relay sets (NIP-51)
/// - 30003: Bookmark sets (NIP-51)
const ALLOWED_KINDS: &[i64] = &[
    0, 1, 3, 6, 7, 16, 9735, 10000, 10002, 10003, 10063, 30000, 30001, 30002, 30003,
];

/// Manages connections to multiple relays and ingests events into the database.
pub struct RelayIngester {
    relay_urls: Vec<String>,
    repo: EventRepository,
    cache: StatsCache,
    since: i64,
    events_ingested: Arc<AtomicU64>,
}

impl RelayIngester {
    pub fn new(
        relay_urls: Vec<String>,
        repo: EventRepository,
        cache: StatsCache,
        since: Option<i64>,
    ) -> Self {
        let since = since.unwrap_or_else(|| chrono::Utc::now().timestamp());
        Self {
            relay_urls,
            repo,
            cache,
            since,
            events_ingested: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Start ingestion: one task per relay, one shared processing worker.
    pub async fn run(self, shutdown: tokio::sync::broadcast::Sender<()>) {
        let (tx, rx) = mpsc::channel::<IngestedEvent>(4096);

        // Spawn processing worker
        let repo = self.repo.clone();
        let cache = self.cache.clone();
        let counter = self.events_ingested.clone();
        let mut shutdown_rx = shutdown.subscribe();
        tokio::spawn(async move {
            Self::process_events(rx, repo, cache, counter, &mut shutdown_rx).await;
        });

        // Spawn one task per relay
        for relay_url in &self.relay_urls {
            let tx = tx.clone();
            let url = relay_url.clone();
            let since = self.since;
            let mut shutdown_rx = shutdown.subscribe();
            tokio::spawn(async move {
                Self::relay_loop(&url, since, tx, &mut shutdown_rx).await;
            });
        }

        tracing::info!(
            relay_count = self.relay_urls.len(),
            since = self.since,
            "relay ingestion started"
        );
    }

    /// Connect to a single relay, subscribe, and forward events. Reconnects on failure.
    async fn relay_loop(
        relay_url: &str,
        since: i64,
        tx: mpsc::Sender<IngestedEvent>,
        shutdown: &mut tokio::sync::broadcast::Receiver<()>,
    ) {
        let mut backoff = Duration::from_secs(1);
        let max_backoff = Duration::from_secs(60);

        loop {
            tracing::info!(relay = relay_url, "connecting");

            match tokio_tungstenite::connect_async(relay_url).await {
                Ok((ws_stream, _)) => {
                    backoff = Duration::from_secs(1);
                    let (mut write, mut read) = ws_stream.split();

                    // Subscribe: only allowed kinds since timestamp
                    let sub_id = format!("nostr-api-{}", &relay_url[6..relay_url.len().min(20)]);
                    let req = serde_json::json!(["REQ", &sub_id, {
                        "since": since,
                        "kinds": ALLOWED_KINDS,
                    }]);
                    if let Err(e) = write.send(Message::Text(req.to_string().into())).await {
                        tracing::warn!(relay = relay_url, error = %e, "failed to send REQ");
                        continue;
                    }

                    tracing::info!(relay = relay_url, sub_id = %sub_id, "subscribed");

                    loop {
                        tokio::select! {
                            msg = read.next() => {
                                match msg {
                                    Some(Ok(Message::Text(text))) => {
                                        if let Some(event) = Self::parse_event_message(&text, relay_url) {
                                            if tx.send(event).await.is_err() {
                                                tracing::error!("event channel closed");
                                                return;
                                            }
                                        }
                                    }
                                    Some(Ok(Message::Close(_))) | None => {
                                        tracing::warn!(relay = relay_url, "connection closed");
                                        break;
                                    }
                                    Some(Err(e)) => {
                                        tracing::warn!(relay = relay_url, error = %e, "ws error");
                                        break;
                                    }
                                    _ => {}
                                }
                            }
                            _ = shutdown.recv() => {
                                tracing::info!(relay = relay_url, "shutting down");
                                let _ = write.send(Message::Close(None)).await;
                                return;
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(relay = relay_url, error = %e, backoff_secs = backoff.as_secs(), "connection failed");
                }
            }

            // Reconnect with backoff
            tokio::select! {
                _ = sleep(backoff) => {}
                _ = shutdown.recv() => return,
            }
            backoff = (backoff * 2).min(max_backoff);
        }
    }

    /// Parse a relay message and extract EVENT payloads.
    fn parse_event_message(text: &str, relay_url: &str) -> Option<IngestedEvent> {
        let parsed: Value = serde_json::from_str(text).ok()?;
        let arr = parsed.as_array()?;

        if arr.len() < 3 {
            return None;
        }

        let msg_type = arr[0].as_str()?;
        if msg_type != "EVENT" {
            if msg_type == "EOSE" {
                let sub_id = arr.get(1).and_then(|v| v.as_str()).unwrap_or("?");
                tracing::debug!(relay = relay_url, sub_id, "EOSE received");
            } else if msg_type == "NOTICE" {
                let notice = arr.get(1).and_then(|v| v.as_str()).unwrap_or("?");
                tracing::info!(relay = relay_url, notice, "NOTICE from relay");
            }
            return None;
        }

        let event: NostrEvent = serde_json::from_value(arr[2].clone()).ok()?;

        // Server-side filter: drop kinds we don't care about (safety net)
        if !ALLOWED_KINDS.contains(&event.kind) {
            return None;
        }

        Some(IngestedEvent {
            event,
            relay_url: relay_url.to_string(),
        })
    }

    /// Process incoming events: deduplicate, store in DB, update cache.
    async fn process_events(
        mut rx: mpsc::Receiver<IngestedEvent>,
        repo: EventRepository,
        cache: StatsCache,
        counter: Arc<AtomicU64>,
        shutdown: &mut tokio::sync::broadcast::Receiver<()>,
    ) {
        loop {
            tokio::select! {
                Some(ingested) = rx.recv() => {
                    match repo.insert_event(&ingested.event, &ingested.relay_url).await {
                        Ok(true) => {
                            let count = counter.fetch_add(1, Ordering::Relaxed) + 1;
                            cache.on_event_ingested(&ingested.event.pubkey, ingested.event.kind).await;
                            if ingested.event.kind == 3 {
                                if let Err(e) = repo.upsert_follow_list(&ingested.event).await {
                                    tracing::error!(error = %e, event_id = %ingested.event.id, "failed to upsert follow list");
                                }
                            }
                            if count % 1000 == 0 {
                                tracing::info!(total = count, "events ingested");
                            }
                        }
                        Ok(false) => {} // duplicate, skip
                        Err(e) => {
                            tracing::error!(error = %e, event_id = %ingested.event.id, "failed to insert event");
                        }
                    }
                }
                _ = shutdown.recv() => {
                    tracing::info!("event processor shutting down");
                    break;
                }
            }
        }
    }
}
