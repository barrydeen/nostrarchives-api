use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio::time::{interval, timeout};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use crate::db::models::NostrEvent;
use crate::db::repository::EventRepository;

const PROFILE_KIND: i64 = 0;

/// How many pubkeys to resolve per batch REQ.
const BATCH_SIZE: usize = 100;

/// How often to drain the queue and resolve (seconds).
const RESOLVE_INTERVAL_SECS: u64 = 30;

/// Timeout waiting for relay responses per batch.
const RELAY_TIMEOUT: Duration = Duration::from_secs(10);

/// Max queued pubkeys before we start dropping (backpressure).
const MAX_QUEUE_SIZE: usize = 50_000;

/// Accepts pubkeys from ingestion and periodically fetches their kind-0
/// metadata from indexer relays.
pub struct MetadataResolver {
    /// Pubkeys waiting to be resolved (deduped).
    pending: Arc<Mutex<HashSet<String>>>,
    /// Pubkeys we've already attempted (avoid re-querying on every event).
    attempted: Arc<Mutex<HashSet<String>>>,
    repo: EventRepository,
    relay_urls: Vec<String>,
}

impl MetadataResolver {
    pub fn new(repo: EventRepository, relay_urls: Vec<String>) -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashSet::new())),
            attempted: Arc::new(Mutex::new(HashSet::new())),
            repo,
            relay_urls,
        }
    }

    /// Returns a sender that ingestion tasks can use to submit pubkeys for resolution.
    pub fn start(self, shutdown: broadcast::Sender<()>) -> mpsc::Sender<String> {
        let (tx, rx) = mpsc::channel::<String>(8192);
        let pending = self.pending.clone();
        let attempted = self.attempted.clone();

        // Spawn the receiver that feeds the queue
        let pending_clone = pending.clone();
        let attempted_clone = attempted.clone();
        let mut shutdown_rx = shutdown.subscribe();
        tokio::spawn(async move {
            Self::queue_receiver(rx, pending_clone, attempted_clone, &mut shutdown_rx).await;
        });

        // Spawn the periodic resolver
        let mut shutdown_rx = shutdown.subscribe();
        tokio::spawn(async move {
            Self::resolve_loop(
                pending,
                attempted,
                self.repo,
                self.relay_urls,
                &mut shutdown_rx,
            )
            .await;
        });

        tx
    }

    /// Receives pubkeys from the channel and adds them to the pending set.
    async fn queue_receiver(
        mut rx: mpsc::Receiver<String>,
        pending: Arc<Mutex<HashSet<String>>>,
        attempted: Arc<Mutex<HashSet<String>>>,
        shutdown: &mut broadcast::Receiver<()>,
    ) {
        loop {
            tokio::select! {
                Some(pubkey) = rx.recv() => {
                    // Skip if already attempted
                    if attempted.lock().await.contains(&pubkey) {
                        continue;
                    }
                    let mut set = pending.lock().await;
                    if set.len() < MAX_QUEUE_SIZE {
                        set.insert(pubkey);
                    }
                }
                _ = shutdown.recv() => {
                    tracing::info!("metadata queue receiver shutting down");
                    return;
                }
            }
        }
    }

    /// Periodically drains pending pubkeys, checks DB for missing metadata,
    /// and fetches from relays.
    async fn resolve_loop(
        pending: Arc<Mutex<HashSet<String>>>,
        attempted: Arc<Mutex<HashSet<String>>>,
        repo: EventRepository,
        relay_urls: Vec<String>,
        shutdown: &mut broadcast::Receiver<()>,
    ) {
        let mut tick = interval(Duration::from_secs(RESOLVE_INTERVAL_SECS));

        loop {
            tokio::select! {
                _ = tick.tick() => {
                    // Drain pending set
                    let batch: Vec<String> = {
                        let mut set = pending.lock().await;
                        let drained: Vec<String> = set.drain().collect();
                        drained
                    };

                    if batch.is_empty() {
                        continue;
                    }

                    // Check which actually need metadata
                    let missing = match repo.pubkeys_missing_metadata(&batch).await {
                        Ok(m) => m,
                        Err(e) => {
                            tracing::error!(error = %e, "failed to check missing metadata");
                            continue;
                        }
                    };

                    if missing.is_empty() {
                        // All had metadata already; mark as attempted
                        let mut att = attempted.lock().await;
                        for pk in &batch {
                            att.insert(pk.clone());
                        }
                        continue;
                    }

                    tracing::info!(
                        pending = batch.len(),
                        missing = missing.len(),
                        "resolving profile metadata"
                    );

                    // Fetch in batches from relays
                    for chunk in missing.chunks(BATCH_SIZE) {
                        let found = fetch_metadata_from_relays(chunk, &relay_urls, &repo).await;
                        tracing::info!(
                            requested = chunk.len(),
                            found = found,
                            "metadata batch resolved"
                        );
                    }

                    // Mark all as attempted (even those not found — don't retry endlessly)
                    let mut att = attempted.lock().await;
                    for pk in &batch {
                        att.insert(pk.clone());
                    }

                    // Periodic cleanup: cap attempted set size
                    if att.len() > 200_000 {
                        tracing::info!(size = att.len(), "trimming attempted set");
                        att.clear();
                    }
                }
                _ = shutdown.recv() => {
                    tracing::info!("metadata resolver shutting down");
                    return;
                }
            }
        }
    }
}

/// Fetch kind-0 metadata for specific pubkeys from relay(s).
/// Tries each relay until we get results or exhaust the list.
/// Returns how many profiles were stored.
async fn fetch_metadata_from_relays(
    pubkeys: &[String],
    relay_urls: &[String],
    repo: &EventRepository,
) -> usize {
    let mut stored = 0usize;
    let mut remaining: HashSet<String> = pubkeys.iter().cloned().collect();

    for relay_url in relay_urls {
        if remaining.is_empty() {
            break;
        }

        let targets: Vec<String> = remaining.iter().cloned().collect();
        match fetch_kind0_from_relay(relay_url, &targets).await {
            Ok(events) => {
                for event in events {
                    remaining.remove(&event.pubkey);
                    match repo.insert_event(&event, relay_url).await {
                        Ok(_) => {
                            stored += 1;
                        }
                        Err(e) => {
                            tracing::warn!(
                                pubkey = %event.pubkey,
                                error = %e,
                                "failed to store metadata event"
                            );
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(relay = relay_url, error = %e, "metadata fetch failed");
            }
        }
    }

    stored
}

/// Connect to a single relay and request kind-0 for the given pubkeys.
async fn fetch_kind0_from_relay(
    relay_url: &str,
    pubkeys: &[String],
) -> Result<Vec<NostrEvent>, String> {
    let (ws_stream, _) = tokio_tungstenite::connect_async(relay_url)
        .await
        .map_err(|e| format!("connect failed: {e}"))?;
    let (mut write, mut read) = ws_stream.split();

    let sub_id = format!("meta-{}", Uuid::new_v4().simple());
    let req = serde_json::json!([
        "REQ",
        &sub_id,
        {
            "kinds": [PROFILE_KIND],
            "authors": pubkeys,
        }
    ]);

    write
        .send(Message::Text(req.to_string().into()))
        .await
        .map_err(|e| format!("send REQ failed: {e}"))?;

    let mut events = Vec::new();

    loop {
        match timeout(RELAY_TIMEOUT, read.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                let parsed: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let arr = match parsed.as_array() {
                    Some(a) if a.len() >= 3 => a,
                    _ => continue,
                };
                let msg_type = match arr[0].as_str() {
                    Some(t) => t,
                    None => continue,
                };

                if msg_type == "EOSE" {
                    break; // All events received
                }

                if msg_type != "EVENT" {
                    continue;
                }

                if let Ok(event) = serde_json::from_value::<NostrEvent>(arr[2].clone()) {
                    if event.kind == PROFILE_KIND {
                        events.push(event);
                    }
                }
            }
            Ok(Some(Ok(Message::Ping(data)))) => {
                let _ = write.send(Message::Pong(data)).await;
            }
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) => break,
            Ok(Some(Err(e))) => return Err(format!("ws error: {e}")),
            Err(_) => break, // timeout
            _ => {}
        }
    }

    // Send CLOSE
    let close_msg = serde_json::json!(["CLOSE", &sub_id]);
    let _ = write
        .send(Message::Text(close_msg.to_string().into()))
        .await;

    Ok(events)
}
