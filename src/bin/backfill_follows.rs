//! Backfill missing follow lists (kind-3) for authors in crawl_state.
//!
//! Finds all pubkeys in crawl_state that have no entry in follow_lists,
//! then fetches their kind-3 from indexer relays. All relays are queried
//! in parallel per batch — fastest response wins for each pubkey.
//!
//! Usage:
//!   cargo run --bin backfill_follows [-- --dry-run --batch-size 500]
//!
//! Flags:
//!   --dry-run       Fetch from relays but skip all DB writes (for testing speed)
//!   --batch-size N  Authors per relay request (default 500)

use std::collections::HashMap;
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
/// Total time budget per batch across all relays racing.
const BATCH_TIMEOUT: Duration = Duration::from_secs(20);
/// Per-relay connect timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Per-relay message read timeout (time waiting for next message after connect).
const READ_TIMEOUT: Duration = Duration::from_secs(10);

const RELAYS: &[&str] = &[
    "wss://indexer.coracle.social",
    "wss://relay.damus.io",
    "wss://relay.primal.net",
    "wss://nos.lol",
    "wss://relay.nos.social",
    "wss://purplepag.es",
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
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let batch_size = parse_flag(&args, "--batch-size")
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_BATCH_SIZE);

    let relays: Vec<String> = RELAYS.iter().map(|s| s.to_string()).collect();

    // Database setup
    let database_url = env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let pool = db::init_pool(&database_url)
        .await
        .expect("failed to connect to database");

    let repo = if !dry_run {
        let follower_cache = FollowerCache::new(pool.clone(), 21, 900);
        let wot_cache = WotCache::new(pool.clone(), 21, 900);
        let block_cache = nostr_api::block_cache::BlockCache::new(pool.clone());
        Some(EventRepository::new(pool.clone(), follower_cache, wot_cache, block_cache))
    } else {
        None
    };

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
        dry_run = dry_run,
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
        let batch_pubkeys: Vec<String> = batch.to_vec();

        // Race all relays in parallel — merge results keeping newest per pubkey
        let events = race_fetch_kind3(&relays, &batch_pubkeys).await;

        let batch_found = events.len();
        let mut batch_upserted: usize = 0;

        if !dry_run {
            if let Some(ref repo) = repo {
                for event in &events {
                    let relay_url = "backfill";
                    match repo.insert_event(event, relay_url).await {
                        Ok(true) => {
                            match repo.upsert_follow_list(event).await {
                                Ok(Some(_)) => batch_upserted += 1,
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
                            // Event exists but follow_lists might be missing
                            match repo.upsert_follow_list(event).await {
                                Ok(Some(_)) => batch_upserted += 1,
                                _ => {}
                            }
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
            }
        }

        authors_processed += batch.len();
        total_found += batch_found;
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
            batch_found = batch_found,
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

/// Race all relays in parallel for a batch of pubkeys.
/// Each relay gets the same full batch. Results are merged keeping the newest
/// kind-3 event per pubkey across all relays.
async fn race_fetch_kind3(relays: &[String], pubkeys: &[String]) -> Vec<NostrEvent> {
    let tasks: Vec<_> = relays
        .iter()
        .map(|relay_url| {
            let relay = relay_url.clone();
            let pks = pubkeys.to_vec();
            tokio::spawn(async move { fetch_kind3_batch(&relay, &pks).await })
        })
        .collect();

    // Wait for all relays with a total batch timeout
    let results = match timeout(BATCH_TIMEOUT, futures_util::future::join_all(tasks)).await {
        Ok(results) => results,
        Err(_) => {
            tracing::warn!("batch timeout reached, using partial results");
            return Vec::new();
        }
    };

    // Merge: keep newest event per pubkey
    let mut best: HashMap<String, NostrEvent> = HashMap::new();
    let mut relay_stats: Vec<(&str, usize)> = Vec::new();

    for (i, result) in results.into_iter().enumerate() {
        let relay_name = relays.get(i).map(|s| s.as_str()).unwrap_or("?");
        match result {
            Ok(Ok(events)) => {
                let count = events.len();
                for event in events {
                    let existing = best.get(&event.pubkey);
                    if existing.is_none() || event.created_at > existing.unwrap().created_at {
                        best.insert(event.pubkey.clone(), event);
                    }
                }
                relay_stats.push((relay_name, count));
            }
            Ok(Err(e)) => {
                tracing::debug!(relay = relay_name, error = %e, "relay failed");
                relay_stats.push((relay_name, 0));
            }
            Err(e) => {
                tracing::debug!(relay = relay_name, error = %e, "relay task panicked");
                relay_stats.push((relay_name, 0));
            }
        }
    }

    tracing::debug!(
        relays = ?relay_stats.iter().map(|(r, c)| format!("{}:{}", r.split('/').last().unwrap_or(r), c)).collect::<Vec<_>>(),
        merged = best.len(),
        "relay race complete"
    );

    best.into_values().collect()
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

    loop {
        match timeout(READ_TIMEOUT, read.next()).await {
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
                                // Keep newest per pubkey
                                if let Some(existing) =
                                    events.iter().position(|e| e.pubkey == event.pubkey)
                                {
                                    if event.created_at > events[existing].created_at {
                                        events[existing] = event;
                                    }
                                } else {
                                    events.push(event);
                                }
                            }
                        }
                    }
                    "EOSE" => break,
                    "CLOSED" => break,
                    _ => {}
                }
            }
            Ok(Some(Ok(Message::Ping(data)))) => {
                let _ = write.send(Message::Pong(data)).await;
            }
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) => break,
            Ok(Some(Err(e))) => return Err(format!("ws error: {e}")),
            Err(_) => break, // read timeout
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
