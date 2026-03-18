use std::collections::HashSet;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{stream, SinkExt, StreamExt};
use serde_json::Value;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use crate::db::models::NostrEvent;
use crate::db::repository::EventRepository;
use crate::error::AppError;

const FOLLOW_LIST_KIND: i64 = 3;
const RELAY_LIST_KIND: i64 = 10002;
const PROFILE_KIND: i64 = 0;

/// Per-relay event cap. Large indexer relays can have 500k+ follow lists.
/// We want as many as possible on the first pass.
const MAX_EVENTS_PER_RELAY: usize = 50_000;
/// Timeout waiting for a single WS message. Relays can be slow under load.
const MESSAGE_TIMEOUT: Duration = Duration::from_secs(15);
/// Connect timeout per relay.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Max concurrent relay connections.
const MAX_CONCURRENCY: usize = 6;

/// Well-known large indexer relays that store follow lists from across the network.
/// These are queried in addition to the user's configured relays to maximize
/// social graph coverage on a fresh database.
const BOOTSTRAP_RELAYS: &[&str] = &[
    "wss://relay.damus.io",
    "wss://relay.primal.net",
    "wss://nos.lol",
    "wss://relay.nostr.band",
    "wss://purplepag.es",
    "wss://relay.nos.social",
    "wss://relay.snort.social",
    "wss://nostr.wine",
    "wss://relay.nostr.bg",
    "wss://nostr-pub.wellorder.net",
];

#[derive(Debug, Default)]
pub struct SocialBootstrapStats {
    pub lists_seen: usize,
    pub updates_applied: usize,
    pub relay_lists_seen: usize,
    pub profiles_seen: usize,
}

#[derive(Debug)]
struct BootstrapError {
    message: String,
}

impl BootstrapError {
    fn new(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
        }
    }
}

impl fmt::Display for BootstrapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for BootstrapError {}

impl From<AppError> for BootstrapError {
    fn from(value: AppError) -> Self {
        BootstrapError::new(value.to_string())
    }
}

/// Aggressive social graph bootstrap that pulls follow lists, relay lists, and
/// profiles from every reachable relay. Runs multiple passes:
///
/// Pass 1: Fetch kind-3 (follow lists) and kind-10002 (relay lists) from all
///          configured relays + hardcoded bootstrap relays. This builds the
///          follow graph and discovers which relays authors prefer.
///
/// Pass 2: Discover additional relays from the kind-10002 data we just ingested,
///          then fetch follow lists from those too (covers authors who only
///          publish to niche relays).
///
/// Pass 3: Fetch kind-0 profiles for all pubkeys we've seen but don't have
///          metadata for yet.
pub async fn bootstrap_social_graph(repo: EventRepository, configured_relays: Vec<String>) {
    if configured_relays.is_empty() {
        return;
    }

    let start = Instant::now();
    let total_lists = Arc::new(AtomicUsize::new(0));
    let total_updates = Arc::new(AtomicUsize::new(0));

    // Build deduplicated relay list: configured + bootstrap relays
    let mut all_relays: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    let bootstrap: Vec<String> = BOOTSTRAP_RELAYS.iter().map(|s| s.to_string()).collect();
    for url in configured_relays.iter().chain(bootstrap.iter()) {
        let normalized = url.trim().to_lowercase().trim_end_matches('/').to_string();
        if seen.insert(normalized.clone()) {
            all_relays.push(normalized);
        }
    }

    tracing::info!(
        relays = all_relays.len(),
        "social graph bootstrap: PASS 1 — fetching follow lists + relay lists"
    );

    // ── Pass 1: Follow lists + relay lists from all known relays ──────────
    run_bootstrap_pass(&repo, &all_relays, &total_lists, &total_updates).await;

    let p1_elapsed = start.elapsed();
    tracing::info!(
        follow_lists = total_lists.load(Ordering::Relaxed),
        updates = total_updates.load(Ordering::Relaxed),
        elapsed_secs = p1_elapsed.as_secs(),
        "social graph bootstrap: PASS 1 complete"
    );

    // ── Pass 2: Discover additional relays from ingested kind-10002 ──────
    let discovered_relays = discover_relays_from_db(&repo).await;
    let mut new_relays: Vec<String> = Vec::new();
    for url in &discovered_relays {
        let normalized = url.trim().to_lowercase().trim_end_matches('/').to_string();
        if seen.insert(normalized.clone()) {
            new_relays.push(normalized);
        }
    }

    if !new_relays.is_empty() {
        tracing::info!(
            new_relays = new_relays.len(),
            "social graph bootstrap: PASS 2 — fetching from discovered relays"
        );
        run_bootstrap_pass(&repo, &new_relays, &total_lists, &total_updates).await;

        tracing::info!(
            follow_lists = total_lists.load(Ordering::Relaxed),
            updates = total_updates.load(Ordering::Relaxed),
            elapsed_secs = start.elapsed().as_secs(),
            "social graph bootstrap: PASS 2 complete"
        );
    } else {
        tracing::info!("social graph bootstrap: PASS 2 skipped (no new relays discovered)");
    }

    tracing::info!(
        total_follow_lists = total_lists.load(Ordering::Relaxed),
        total_updates = total_updates.load(Ordering::Relaxed),
        elapsed_secs = start.elapsed().as_secs(),
        "social graph bootstrap: COMPLETE"
    );
}

/// Run one pass of the bootstrap against a set of relays.
async fn run_bootstrap_pass(
    repo: &EventRepository,
    relays: &[String],
    total_lists: &Arc<AtomicUsize>,
    total_updates: &Arc<AtomicUsize>,
) {
    let mut tasks = stream::iter(relays.iter().cloned())
        .map(|relay| {
            let repo = repo.clone();
            let total_lists = Arc::clone(total_lists);
            let total_updates = Arc::clone(total_updates);
            async move {
                match fetch_social_data(&relay, repo).await {
                    Ok(stats) => {
                        total_lists.fetch_add(stats.lists_seen, Ordering::Relaxed);
                        total_updates.fetch_add(stats.updates_applied, Ordering::Relaxed);
                        tracing::info!(
                            relay = relay.as_str(),
                            follow_lists = stats.lists_seen,
                            relay_lists = stats.relay_lists_seen,
                            profiles = stats.profiles_seen,
                            updates = stats.updates_applied,
                            "social graph relay synced"
                        );
                    }
                    Err(err) => {
                        tracing::warn!(relay = relay.as_str(), error = %err, "social graph relay failed");
                    }
                }
            }
        })
        .buffer_unordered(MAX_CONCURRENCY);

    while tasks.next().await.is_some() {}
}

/// Fetch follow lists (kind 3), relay lists (kind 10002), and profiles (kind 0)
/// from a single relay.
async fn fetch_social_data(
    relay_url: &str,
    repo: EventRepository,
) -> Result<SocialBootstrapStats, BootstrapError> {
    let (ws_stream, _) = timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(relay_url))
        .await
        .map_err(|_| BootstrapError::new(format!("connect timeout: {relay_url}")))?
        .map_err(|e| BootstrapError::new(format!("connect failed: {e}")))?;
    let (mut write, mut read) = ws_stream.split();

    // Request follow lists, relay lists, and profiles in one subscription
    let sub_id = format!("social-bootstrap-{}", Uuid::new_v4().simple());
    let req = serde_json::json!([
        "REQ",
        &sub_id,
        {
            "kinds": [FOLLOW_LIST_KIND, RELAY_LIST_KIND, PROFILE_KIND],
            "limit": MAX_EVENTS_PER_RELAY as i64,
        }
    ]);

    write
        .send(Message::Text(req.to_string().into()))
        .await
        .map_err(|e| BootstrapError::new(format!("failed to send REQ: {e}")))?;

    let mut stats = SocialBootstrapStats::default();
    let mut total_events = 0usize;

    loop {
        match timeout(MESSAGE_TIMEOUT, read.next()).await {
            Ok(Some(Ok(message))) => match message {
                Message::Text(payload) => {
                    if let Some(event) = parse_event(&payload) {
                        total_events += 1;

                        match event.kind {
                            3 => {
                                stats.lists_seen += 1;
                                // insert_event handles kind-3 upsert internally now
                                if let Ok(true) = repo.insert_event(&event, relay_url).await {
                                    if repo.upsert_follow_list(&event).await.ok().flatten().is_some() {
                                        stats.updates_applied += 1;
                                    }
                                }
                            }
                            10002 => {
                                stats.relay_lists_seen += 1;
                                // insert_event handles kind-10002 upsert internally
                                let _ = repo.insert_event(&event, relay_url).await;
                            }
                            0 => {
                                stats.profiles_seen += 1;
                                let _ = repo.insert_event(&event, relay_url).await;
                            }
                            _ => {}
                        }

                        if total_events >= MAX_EVENTS_PER_RELAY {
                            break;
                        }

                        // Log progress every 10k events
                        if total_events % 10_000 == 0 {
                            tracing::info!(
                                relay = relay_url,
                                events = total_events,
                                follow_lists = stats.lists_seen,
                                "social graph bootstrap: progress"
                            );
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
            Ok(Some(Err(e))) => return Err(BootstrapError::new(format!("ws error: {e}"))),
            Ok(None) => break,
            Err(_) => break, // message timeout = relay has nothing more to send
        }
    }

    // Clean close
    let close_msg = serde_json::json!(["CLOSE", &sub_id]);
    let _ = write.send(Message::Text(close_msg.to_string().into())).await;
    let _ = write.close().await;

    Ok(stats)
}

/// Discover relay URLs from kind-10002 events already in the database.
/// Returns the top relays by user count that we haven't tried yet.
async fn discover_relays_from_db(repo: &EventRepository) -> Vec<String> {
    let result = sqlx::query_as::<_, (String,)>(
        r#"
        WITH latest_relay_lists AS (
            SELECT DISTINCT ON (pubkey) pubkey, tags
            FROM events
            WHERE kind = 10002
            ORDER BY pubkey, created_at DESC
        ),
        relay_urls AS (
            SELECT
                RTRIM(LOWER(tag ->> 1), '/') AS relay_url
            FROM latest_relay_lists,
                 jsonb_array_elements(tags::jsonb) AS tag
            WHERE tag ->> 0 = 'r'
              AND tag ->> 1 IS NOT NULL
              AND tag ->> 1 != ''
        )
        SELECT relay_url
        FROM relay_urls
        WHERE relay_url LIKE 'wss://%'
        GROUP BY relay_url
        HAVING COUNT(*) >= 10
        ORDER BY COUNT(*) DESC
        LIMIT 30
        "#,
    )
    .fetch_all(&repo.pool())
    .await;

    match result {
        Ok(rows) => rows.into_iter().map(|(url,)| url).collect(),
        Err(e) => {
            tracing::warn!(error = %e, "failed to discover relays from DB");
            Vec::new()
        }
    }
}

fn parse_event(text: &str) -> Option<NostrEvent> {
    let parsed: Value = serde_json::from_str(text).ok()?;
    let arr = parsed.as_array()?;

    if arr.len() < 3 {
        return None;
    }

    if arr.first().and_then(|v| v.as_str())? != "EVENT" {
        return None;
    }

    let event: NostrEvent = serde_json::from_value(arr[2].clone()).ok()?;
    if !matches!(event.kind, 0 | 3 | 10002) {
        return None;
    }

    Some(event)
}
