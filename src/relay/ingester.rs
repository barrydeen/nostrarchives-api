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
/// v2: Removed NIP-51 lists (10000, 10003, 10063, 30000-30003) -- not used by any endpoint.
/// Kept 6/7/16 for real-time counter increments (processed as counters, not stored).
///
/// - 0:     Profile metadata (NIP-01)
/// - 1:     Short text note (NIP-01)
/// - 3:     Contact list (NIP-02) -- social graph
/// - 6:     Repost (NIP-18) -- counter only
/// - 7:     Reaction (NIP-25) -- counter only
/// - 16:    Generic repost (NIP-18) -- counter only
/// - 9735:  Zap receipt (NIP-57) -- always stored
/// - 10002: Relay list (NIP-65) -- upsert only
const ALLOWED_KINDS: &[i64] = &[0, 1, 3, 6, 7, 16, 9735, 10002];

/// Manages connections to multiple relays and ingests events into the database.
pub struct RelayIngester {
    relay_urls: Vec<String>,
    repo: EventRepository,
    cache: StatsCache,
    since: i64,
    events_ingested: Arc<AtomicU64>,
    metadata_tx: Option<mpsc::Sender<String>>,
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
            metadata_tx: None,
        }
    }

    /// Attach a metadata resolver sender. Discovered pubkeys will be queued for
    /// kind-0 resolution.
    pub fn with_metadata_sender(mut self, tx: mpsc::Sender<String>) -> Self {
        self.metadata_tx = Some(tx);
        self
    }

    /// Start ingestion: one task per relay, one shared processing worker.
    pub async fn run(self, shutdown: tokio::sync::broadcast::Sender<()>) {
        let (tx, rx) = mpsc::channel::<IngestedEvent>(4096);

        // Spawn processing worker
        let repo = self.repo.clone();
        let cache = self.cache.clone();
        let counter = self.events_ingested.clone();
        let metadata_tx = self.metadata_tx.clone();
        let mut shutdown_rx = shutdown.subscribe();
        tokio::spawn(async move {
            Self::process_events(rx, repo, cache, counter, metadata_tx, &mut shutdown_rx).await;
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
    /// v2: kind-based routing -- reactions/reposts are counter-only,
    /// kind-3/10002 are upsert-only.
    async fn process_events(
        mut rx: mpsc::Receiver<IngestedEvent>,
        repo: EventRepository,
        cache: StatsCache,
        counter: Arc<AtomicU64>,
        metadata_tx: Option<mpsc::Sender<String>>,
        shutdown: &mut tokio::sync::broadcast::Receiver<()>,
    ) {
        loop {
            tokio::select! {
                Some(ingested) = rx.recv() => {
                    match repo.insert_event(&ingested.event, &ingested.relay_url).await {
                        Ok(true) => {
                            let count = counter.fetch_add(1, Ordering::Relaxed) + 1;
                            cache.on_event_ingested(&ingested.event.pubkey, ingested.event.kind).await;

                            // For zap receipts, extract the sat amount and record in live metrics
                            if ingested.event.kind == 9735 {
                                let zap_sats = Self::extract_zap_amount(&ingested.event);
                                if zap_sats > 0 {
                                    cache.record_live_zap(zap_sats).await;
                                }
                            }

                            // Queue author pubkey for metadata resolution
                            // (skip for counter-only kinds 6/7/16 since we don't store them)
                            if !matches!(ingested.event.kind, 6 | 7 | 16) {
                                if let Some(ref tx) = metadata_tx {
                                    let _ = tx.try_send(ingested.event.pubkey.clone());
                                }
                            }

                            // Kind-3: upsert follow list (social graph)
                            if ingested.event.kind == 3 {
                                if let Err(e) = repo.upsert_follow_list(&ingested.event).await {
                                    tracing::error!(error = %e, event_id = %ingested.event.id, "failed to upsert follow list");
                                }
                                // Also queue pubkeys from follow lists
                                if let Some(ref tx) = metadata_tx {
                                    for tag in &ingested.event.tags {
                                        if tag.first().map(|v| v == "p").unwrap_or(false) {
                                            if let Some(target) = tag.get(1).filter(|v| !v.is_empty()) {
                                                let _ = tx.try_send(target.clone());
                                            }
                                        }
                                    }
                                }
                            }

                            if count % 1000 == 0 {
                                tracing::info!(total = count, "events ingested");
                            }
                        }
                        Ok(false) => {} // duplicate or skipped
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

    /// Extract zap amount in sats from a kind-9735 event.
    ///
    /// Mirrors the amount extraction logic in `EventRepository::extract_zap_metadata`:
    /// 1. Parse the embedded "description" tag (kind-9734 zap request) for an "amount" tag (msats)
    /// 2. Fall back to parsing the "bolt11" tag if the description amount is missing/zero
    ///
    /// Returns sats (msats / 1000), or 0 if the amount cannot be determined.
    fn extract_zap_amount(event: &NostrEvent) -> i64 {
        let mut amount_msats: i64 = 0;

        // Try the description tag first (embedded kind-9734 zap request)
        if let Some(desc_tag) = event.tags.iter().find(|t| t.len() >= 2 && t[0] == "description") {
            if let Ok(zap_request) = serde_json::from_str::<serde_json::Value>(&desc_tag[1]) {
                if let Some(tags) = zap_request.get("tags").and_then(|t| t.as_array()) {
                    for tag in tags {
                        let Some(arr) = tag.as_array() else { continue };
                        if arr.len() >= 2 && arr[0].as_str() == Some("amount") {
                            if let Some(raw) = arr[1].as_str() {
                                if let Ok(parsed) = raw.parse::<i64>() {
                                    amount_msats = parsed;
                                }
                                break;
                            }
                        }
                    }
                }
            }
        }

        // Fall back to bolt11 tag
        if amount_msats == 0 {
            if let Some(bolt11_tag) = event.tags.iter().find(|t| t.len() >= 2 && t[0] == "bolt11") {
                if let Some(bolt11) = bolt11_tag.get(1) {
                    amount_msats = Self::parse_bolt11_amount_msats(bolt11);
                }
            }
        }

        amount_msats / 1000 // convert msats to sats
    }

    /// Parse bolt11 invoice amount to millisatoshis.
    fn parse_bolt11_amount_msats(bolt11: &str) -> i64 {
        use regex::Regex;

        let Ok(re) = Regex::new(r"lnbc(\d+)([munp])1") else {
            return 0;
        };
        let Some(captures) = re.captures(bolt11) else {
            return 0;
        };

        let Some(amount_str) = captures.get(1).map(|m| m.as_str()) else {
            return 0;
        };
        let Ok(amount) = amount_str.parse::<u64>() else {
            return 0;
        };

        let Some(multiplier) = captures.get(2).map(|m| m.as_str()) else {
            return 0;
        };

        let btc_amount = match multiplier {
            "m" => amount as f64 * 1e-3,  // milli
            "u" => amount as f64 * 1e-6,  // micro
            "n" => amount as f64 * 1e-9,  // nano
            "p" => amount as f64 * 1e-12, // pico
            _ => return 0,
        };

        // Convert BTC to msats (1 BTC = 100_000_000_000 msats)
        (btc_amount * 100_000_000_000.0) as i64
    }
}
