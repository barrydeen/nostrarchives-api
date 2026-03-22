//! Backfill missing follow lists (kind-3) for authors in crawl_state.
//!
//! Finds all pubkeys in crawl_state that have no entry in follow_lists,
//! then fetches their kind-3 from relays in large batches.
//!
//! Usage:
//!   cargo run --bin backfill_follows [-- --batch-size 500 --relay wss://relay.damus.io]

use std::env;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use nostr_api::db;
use nostr_api::db::models::NostrEvent;
use nostr_api::db::repository::EventRepository;
use nostr_api::follower_cache::FollowerCache;
use nostr_api::wot_cache::WotCache;

const DEFAULT_BATCH_SIZE: usize = 500;
const RELAY_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

const DEFAULT_RELAYS: &[&str] = &[
    "wss://relay.damus.io",
    "wss://nos.lol",
    "wss://relay.nostr.band",
    "wss://purplepag.es",
    "wss://relay.snort.social",
];

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "backfill_follows=info,nostr_api=warn".into()),
        )
        .init();

    let args: Vec<String> = env::args().collect();
    let batch_size = parse_flag(&args, "--batch-size")
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_BATCH_SIZE);

    let relays: Vec<String> = {
        let mut custom = Vec::new();
        let mut i = 1;
        while i < args.len() {
            if args[i] == "--relay" {
                if let Some(url) = args.get(i + 1) {
                    custom.push(url.clone());
                    i += 2;
                    continue;
                }
            }
            i += 1;
        }
        if custom.is_empty() {
            DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect()
        } else {
            custom
        }
    };

    // Database setup
    let database_url =
        env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let pool = db::init_pool(&database_url)
        .await
        .expect("failed to connect to database");

    let follower_cache = FollowerCache::new(pool.clone(), 21, 900);
    let wot_cache = WotCache::new(pool.clone(), 21, 900);
    let repo = EventRepository::new(pool.clone(), follower_cache, wot_cache);

    // Find all pubkeys missing follow lists
    let missing: Vec<String> = sqlx::query_scalar(
        "SELECT cs.pubkey FROM crawl_state cs \
         LEFT JOIN follow_lists fl ON cs.pubkey = fl.pubkey \
         WHERE fl.pubkey IS NULL \
         ORDER BY cs.priority_tier ASC, cs.follower_count DESC",
    )
    .fetch_all(&pool)
    .await
    .expect("failed to query missing follow lists");

    tracing::info!(
        missing = missing.len(),
        batch_size = batch_size,
        relays = relays.len(),
        "starting follow list backfill"
    );

    if missing.is_empty() {
        tracing::info!("no missing follow lists, nothing to do");
        return;
    }

    let start = Instant::now();
    let total_missing = missing.len();
    let mut total_found: usize = 0;
    let mut total_upserted: usize = 0;
    let mut authors_processed: usize = 0;

    for (batch_idx, batch) in missing.chunks(batch_size).enumerate() {
        let batch_start = Instant::now();
        let mut batch_upserted: usize = 0;
        let mut found_pubkeys = std::collections::HashSet::new();

        // Try each relay until we've found lists for most of the batch
        for relay_url in &relays {
            // Only request pubkeys we haven't found yet
            let still_missing: Vec<String> = batch
                .iter()
                .filter(|pk| !found_pubkeys.contains(*pk))
                .cloned()
                .collect();

            if still_missing.is_empty() {
                break;
            }

            match fetch_kind3_batch(relay_url, &still_missing).await {
                Ok(events) => {
                    for event in &events {
                        found_pubkeys.insert(event.pubkey.clone());

                        // Insert the event (kind-3 always passes WoT gate)
                        match repo.insert_event(event, relay_url).await {
                            Ok(true) => {
                                // Also upsert the follow list
                                match repo.upsert_follow_list(event).await {
                                    Ok(Some(count)) => {
                                        batch_upserted += 1;
                                        tracing::debug!(
                                            pubkey = %&event.pubkey[..12],
                                            follows = count,
                                            "follow list upserted"
                                        );
                                    }
                                    Ok(None) => {}
                                    Err(e) => {
                                        tracing::warn!(
                                            pubkey = %&event.pubkey[..12],
                                            error = %e,
                                            "upsert_follow_list failed"
                                        );
                                    }
                                }
                            }
                            Ok(false) => {
                                // Duplicate or older — still try upsert in case follow_lists
                                // entry is missing despite having the event
                                let _ = repo.upsert_follow_list(event).await;
                            }
                            Err(e) => {
                                tracing::warn!(
                                    pubkey = %&event.pubkey[..12],
                                    error = %e,
                                    "insert_event failed"
                                );
                            }
                        }
                    }
                    tracing::debug!(
                        relay = %relay_url,
                        requested = still_missing.len(),
                        found = events.len(),
                        "relay batch complete"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        relay = %relay_url,
                        error = %e,
                        "fetch failed, trying next relay"
                    );
                }
            }
        }

        authors_processed += batch.len();
        total_found += found_pubkeys.len();
        total_upserted += batch_upserted;

        let elapsed = start.elapsed().as_secs();
        let rate = if elapsed > 0 {
            authors_processed as f64 / elapsed as f64
        } else {
            0.0
        };
        let remaining_authors = total_missing - authors_processed;
        let eta_secs = if rate > 0.0 {
            (remaining_authors as f64 / rate) as u64
        } else {
            0
        };

        tracing::info!(
            batch = batch_idx + 1,
            batch_size = batch.len(),
            batch_found = found_pubkeys.len(),
            batch_upserted = batch_upserted,
            batch_ms = batch_start.elapsed().as_millis() as u64,
            progress = format!("{}/{}", authors_processed, total_missing),
            pct = format!("{:.1}%", authors_processed as f64 / total_missing as f64 * 100.0),
            total_found = total_found,
            total_upserted = total_upserted,
            eta_secs = eta_secs,
            "batch complete"
        );
    }

    tracing::info!(
        total_missing = total_missing,
        total_found = total_found,
        total_upserted = total_upserted,
        hit_rate = format!("{:.1}%", total_found as f64 / total_missing as f64 * 100.0),
        elapsed_secs = start.elapsed().as_secs(),
        "backfill complete"
    );
}

/// Fetch kind-3 events for a batch of authors from a single relay.
async fn fetch_kind3_batch(
    relay_url: &str,
    pubkeys: &[String],
) -> Result<Vec<NostrEvent>, String> {
    let (ws_stream, _) = timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(relay_url))
        .await
        .map_err(|_| format!("connect timeout: {relay_url}"))?
        .map_err(|e| format!("connect failed: {e}"))?;

    let (mut write, mut read) = ws_stream.split();

    let sub_id = format!("bf-{}", Uuid::new_v4().as_simple());
    let req = serde_json::json!([
        "REQ",
        &sub_id,
        {
            "kinds": [3],
            "authors": pubkeys,
        }
    ]);

    write
        .send(Message::Text(req.to_string().into()))
        .await
        .map_err(|e| format!("send REQ failed: {e}"))?;

    let mut events: Vec<NostrEvent> = Vec::new();
    let mut seen_pubkeys = std::collections::HashSet::new();

    loop {
        match timeout(RELAY_TIMEOUT, read.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                let parsed: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let arr = match parsed.as_array() {
                    Some(a) if a.len() >= 2 => a,
                    _ => continue,
                };
                let msg_type = match arr[0].as_str() {
                    Some(t) => t,
                    None => continue,
                };

                match msg_type {
                    "EVENT" if arr.len() >= 3 => {
                        if let Ok(event) = serde_json::from_value::<NostrEvent>(arr[2].clone()) {
                            if event.kind == 3 {
                                // Keep only the newest per pubkey
                                if let Some(existing) =
                                    events.iter().position(|e| e.pubkey == event.pubkey)
                                {
                                    if event.created_at > events[existing].created_at {
                                        events[existing] = event;
                                    }
                                } else {
                                    seen_pubkeys.insert(event.pubkey.clone());
                                    events.push(event);
                                }
                            }
                        }
                    }
                    "EOSE" => break,
                    "CLOSED" | "NOTICE" => {
                        let msg = arr.get(1).and_then(|v| v.as_str()).unwrap_or("");
                        tracing::debug!(relay = relay_url, msg_type, msg, "relay message");
                        if msg_type == "CLOSED" {
                            break;
                        }
                    }
                    _ => {}
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

    // Clean close
    let close_msg = serde_json::json!(["CLOSE", &sub_id]);
    let _ = write.send(Message::Text(close_msg.to_string().into())).await;
    let _ = write.send(Message::Close(None)).await;

    Ok(events)
}

fn parse_flag(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1).cloned())
}
