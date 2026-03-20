//! Diagnostic + manual crawl tool for troubleshooting per-author note coverage.
//!
//! Usage:
//!   cargo run --bin crawl_author -- <command> <pubkey> [args]
//!
//! Commands:
//!   info  <pubkey>              Show DB state: note count, relay list, crawl cursor
//!   probe <pubkey> [relay…]     Count notes per relay without inserting into DB
//!   crawl <pubkey> [relay…]     Fetch and insert all notes from relays into DB
//!
//! <pubkey> may be a 64-char hex pubkey or an npub1… bech32 string.
//! If relay URLs are provided they are used exclusively; otherwise the author's
//! NIP-65 write relays are used (from DB first, then fetched live from bootstrap relays).

use std::collections::HashSet;
use std::env;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(env::var("RUST_LOG").unwrap_or_else(|_| "warn".into()))
        .init();

    let args: Vec<String> = env::args().collect();
    let command = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    match command {
        "info" | "probe" | "crawl" => {}
        _ => {
            print_help();
            return;
        }
    }

    let pubkey_input = args.get(2).unwrap_or_else(|| {
        print_help();
        std::process::exit(1);
    });
    let pubkey = resolve_pubkey(pubkey_input);
    let extra_relays: Vec<String> = args[3..].to_vec();

    let database_url = env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://dev:dev@localhost:5432/nostr_api".into());

    let pool = nostr_api::db::init_pool(&database_url)
        .await
        .expect("failed to connect to database");

    match command {
        "info" => cmd_info(&pool, &pubkey).await,
        "probe" => cmd_probe(&pool, &pubkey, extra_relays).await,
        "crawl" => cmd_crawl(&pool, &pubkey, extra_relays).await,
        _ => unreachable!(),
    }
}

fn print_help() {
    println!("crawl_author — per-author crawl diagnostic tool");
    println!();
    println!("Commands:");
    println!("  info  <pubkey>              Show DB state: note count, relay list, crawl cursor");
    println!("  probe <pubkey> [relay…]     Count notes per relay, no DB changes");
    println!("  crawl <pubkey> [relay…]     Fetch and insert all notes from relays");
    println!();
    println!("  <pubkey> may be hex (64 chars) or npub1… bech32.");
    println!("  If relay URLs are omitted, the author's NIP-65 write relays are used");
    println!("  (from DB, or fetched live from bootstrap relays).");
    println!();
    println!("Examples:");
    println!("  cargo run --bin crawl_author -- info npub1abc...");
    println!("  cargo run --bin crawl_author -- probe npub1abc...");
    println!("  cargo run --bin crawl_author -- probe npub1abc... wss://relay.damus.io");
    println!("  cargo run --bin crawl_author -- crawl npub1abc... wss://nostr.mom");
}

fn resolve_pubkey(input: &str) -> String {
    let trimmed = input.trim();

    // 64-char hex
    if trimmed.len() == 64 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        return trimmed.to_string();
    }

    // npub1… bech32
    if trimmed.starts_with("npub1") {
        use bech32::FromBase32;
        if let Ok((_, data, _)) = bech32::decode(trimmed) {
            if let Ok(bytes) = Vec::<u8>::from_base32(&data) {
                if bytes.len() == 32 {
                    return hex::encode(bytes);
                }
            }
        }
    }

    eprintln!("Error: could not parse pubkey '{input}'.");
    eprintln!("  Expected 64-char hex or npub1… bech32.");
    std::process::exit(1);
}

// ── Commands ─────────────────────────────────────────────────────────────────

/// Show what we currently have in the DB for this author.
async fn cmd_info(pool: &sqlx::PgPool, pubkey: &str) {
    println!("\n=== Author Info ===");
    println!("Pubkey: {pubkey}");
    println!();

    // Note count + date range
    let (count, oldest, newest) = sqlx::query_as::<_, (i64, i64, i64)>(
        "SELECT COUNT(*), COALESCE(MIN(created_at), 0), COALESCE(MAX(created_at), 0)
         FROM events WHERE pubkey = $1 AND kind = 1",
    )
    .bind(pubkey)
    .fetch_one(pool)
    .await
    .unwrap_or((0, 0, 0));

    println!("Kind-1 notes in DB: {count}");
    if oldest > 0 {
        let oldest_dt = fmt_ts(oldest);
        let newest_dt = fmt_ts(newest);
        println!("Date range:         {oldest_dt} → {newest_dt}");
    }

    // Crawl state
    println!();
    let crawl = sqlx::query_as::<_, (i64, i16, Option<i64>, Option<i64>, i64)>(
        "SELECT follower_count, priority_tier, crawl_cursor, newest_seen_at, notes_crawled
         FROM crawl_state WHERE pubkey = $1",
    )
    .bind(pubkey)
    .fetch_optional(pool)
    .await
    .unwrap_or(None);

    if let Some((followers, tier, cursor, newest_seen, notes_crawled)) = crawl {
        println!("Crawl state:");
        println!("  Followers:      {followers}");
        println!("  Priority tier:  {tier}  (1=top, 4=lowest)");
        println!("  Notes crawled:  {notes_crawled}");
        match cursor {
            Some(ts) => println!("  Backfill cursor: {} (oldest fetched so far)", fmt_ts(ts)),
            None => println!("  Backfill cursor: none (backfill not started)"),
        }
        match newest_seen {
            Some(ts) => println!("  Newest seen:    {}", fmt_ts(ts)),
            None => println!("  Newest seen:    none"),
        }
    } else {
        println!("Crawl state: not in crawl queue");
        println!("  (author may not be followed by anyone in our WoT)");
    }

    // NIP-65 relay list from DB
    println!();
    let relay_row = sqlx::query_as::<_, (serde_json::Value,)>(
        "SELECT tags FROM events WHERE pubkey = $1 AND kind = 10002
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(pubkey)
    .fetch_optional(pool)
    .await
    .unwrap_or(None);

    if let Some((tags,)) = relay_row {
        let relays = parse_relay_tags_json(tags.as_array().unwrap_or(&vec![]));
        println!("NIP-65 relay list (in DB): {} relays", relays.len());
        let write_count = relays.iter().filter(|(_, _, w)| *w).count();
        let read_count = relays.iter().filter(|(_, r, _)| *r).count();
        println!("  {write_count} write, {read_count} read");
        for (url, read, write) in &relays {
            let mode = match (read, write) {
                (true, true) => "read+write",
                (true, false) => "read      ",
                (false, true) => "write     ",
                _ => "?         ",
            };
            println!("  [{mode}] {url}");
        }
    } else {
        println!("NIP-65 relay list: not in DB");
        println!("  Run `probe` to discover relays from the network.");
    }
}

/// Probe each of the author's write relays: count notes, compare to what we have.
async fn cmd_probe(pool: &sqlx::PgPool, pubkey: &str, extra_relays: Vec<String>) {
    println!("\n=== Probe: {} ===\n", abbrev(pubkey));

    let relays = resolve_relays(pool, pubkey, extra_relays).await;
    if relays.is_empty() {
        println!("No relays to probe. Try passing relay URLs explicitly:");
        println!("  cargo run --bin crawl_author -- probe <pubkey> wss://relay.damus.io");
        return;
    }

    let db_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM events WHERE pubkey = $1 AND kind = 1")
            .bind(pubkey)
            .fetch_one(pool)
            .await
            .unwrap_or(0);

    println!("Notes currently in DB: {db_count}");
    println!("Probing {} relay(s)...\n", relays.len());

    // (relay_url, note_ids_on_relay, negentropy_supported)
    let mut relay_results: Vec<(String, Vec<String>, bool)> = Vec::new();

    for relay_url in &relays {
        print!("  {relay_url}");

        // Check negentropy support (cached in DB)
        let has_neg = nostr_api::crawler::relay_caps::check_and_update_caps(pool, relay_url)
            .await
            .map(|c| c.supports_negentropy)
            .unwrap_or(false);

        let neg_tag = if has_neg { " [negentropy]" } else { "" };

        match fetch_all_author_note_ids(relay_url, pubkey).await {
            Ok(ids) => {
                let on_relay = ids.len();
                let already = count_ids_in_db(pool, &ids).await;
                let gap = on_relay.saturating_sub(already);
                println!(
                    "{neg_tag}\n    {on_relay} notes on relay  |  {already} in DB  |  {gap} missing"
                );
                relay_results.push((relay_url.clone(), ids, has_neg));
            }
            Err(e) => {
                println!("{neg_tag}\n    ERROR: {e}");
                relay_results.push((relay_url.clone(), vec![], has_neg));
            }
        }
    }

    // Aggregate unique IDs across all relays
    let mut all_ids: HashSet<String> = HashSet::new();
    for (_, ids, _) in &relay_results {
        all_ids.extend(ids.iter().cloned());
    }

    let total_unique = all_ids.len();
    let total_already = count_ids_in_db(pool, &all_ids.iter().cloned().collect::<Vec<_>>()).await;
    let total_gap = total_unique.saturating_sub(total_already);

    println!("\n=== Summary ===");
    println!("  Relays probed:              {}", relays.len());
    println!("  Unique notes across relays: {total_unique}");
    println!("  Already in DB:              {total_already}");
    println!("  Missing from DB (gap):      {total_gap}");
    println!("  DB total for this author:   {db_count}");

    if total_gap > 0 {
        println!();
        println!("To fetch missing notes, run:");
        println!("  cargo run --bin crawl_author -- crawl {}", abbrev(pubkey));
    }

    // Flag any relays without negentropy (useful for understanding why we miss them)
    let no_neg: Vec<&str> = relay_results
        .iter()
        .filter(|(_, _, has_neg)| !has_neg)
        .map(|(url, _, _)| url.as_str())
        .collect();
    if !no_neg.is_empty() {
        println!();
        println!("Note: the following relays do NOT support negentropy — they are only");
        println!("reached via legacy REQ crawling (Phase 2/3), which may be slower:");
        for url in no_neg {
            println!("  {url}");
        }
    }
}

/// Fetch notes from all write relays and insert into DB.
async fn cmd_crawl(pool: &sqlx::PgPool, pubkey: &str, extra_relays: Vec<String>) {
    println!("\n=== Crawl: {} ===\n", abbrev(pubkey));

    let relays = resolve_relays(pool, pubkey, extra_relays).await;
    if relays.is_empty() {
        println!("No relays found. Pass relay URLs directly:");
        println!("  cargo run --bin crawl_author -- crawl <pubkey> wss://relay.damus.io");
        return;
    }

    let redis_url = env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());
    let redis_client = redis::Client::open(redis_url.as_str()).expect("invalid redis url");
    let follower_cache = nostr_api::follower_cache::FollowerCache::new(pool.clone(), 5, 3600);
    let wot_cache = nostr_api::wot_cache::WotCache::new(pool.clone(), 21, 900);
    let repo = nostr_api::db::repository::EventRepository::new(
        pool.clone(),
        follower_cache,
        wot_cache,
    );
    let cache = nostr_api::cache::StatsCache::new(redis_client, repo.clone());
    let syncer =
        nostr_api::crawler::negentropy::NegentropySyncer::new(repo.clone(), cache.clone(), pool.clone());

    let before: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM events WHERE pubkey = $1 AND kind = 1")
            .bind(pubkey)
            .fetch_one(pool)
            .await
            .unwrap_or(0);

    println!("Notes in DB before crawl: {before}");
    println!("Crawling {} relay(s)...\n", relays.len());

    let author_vec = vec![pubkey.to_string()];
    let mut relay_inserted: Vec<(String, usize)> = Vec::new();

    for relay_url in &relays {
        println!("--- {relay_url} ---");

        let has_neg = nostr_api::crawler::relay_caps::check_and_update_caps(pool, relay_url)
            .await
            .map(|c| c.supports_negentropy)
            .unwrap_or(false);

        let inserted = if has_neg {
            println!("  Strategy: negentropy set reconciliation");
            match syncer.sync_authors(relay_url, &[1], &author_vec).await {
                Ok(stats) => {
                    println!("  Discovered (relay has, we don't): {}", stats.events_discovered);
                    println!("  Fetched:                          {}", stats.events_fetched);
                    println!("  Inserted into DB:                 {}", stats.events_inserted);
                    stats.events_inserted
                }
                Err(e) => {
                    println!("  Negentropy failed ({e}), falling back to paginated REQ...");
                    let n = crawl_via_paginated_req(relay_url, pubkey, &repo, &cache).await;
                    println!("  Inserted (REQ fallback): {n}");
                    n
                }
            }
        } else {
            println!("  Strategy: paginated REQ (no negentropy)");
            let n = crawl_via_paginated_req(relay_url, pubkey, &repo, &cache).await;
            println!("  Inserted: {n}");
            n
        };

        relay_inserted.push((relay_url.clone(), inserted));
        println!();
    }

    let after: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM events WHERE pubkey = $1 AND kind = 1")
            .bind(pubkey)
            .fetch_one(pool)
            .await
            .unwrap_or(0);

    println!("=== Crawl complete ===");
    println!("  Notes before: {before}");
    println!("  Notes after:  {after}");
    println!("  Net new:      {}", after - before);
}

// ── Relay resolution ─────────────────────────────────────────────────────────

/// Return the list of relay URLs to use, in priority order:
///   1. Explicitly passed on command line
///   2. Author's NIP-65 write relays from DB
///   3. Live-fetched NIP-65 from bootstrap relays
///   4. Bootstrap relays themselves as a last resort
async fn resolve_relays(
    pool: &sqlx::PgPool,
    pubkey: &str,
    explicit: Vec<String>,
) -> Vec<String> {
    if !explicit.is_empty() {
        return explicit;
    }

    // Try DB
    let db_relays = write_relays_from_db(pool, pubkey).await;
    if !db_relays.is_empty() {
        println!("Using NIP-65 write relays from DB ({} relay(s)):", db_relays.len());
        for r in &db_relays {
            println!("  {r}");
        }
        println!();
        return db_relays;
    }

    // Live fetch from bootstrap relays
    println!("NIP-65 not in DB — fetching live from bootstrap relays...");
    let bootstrap = [
        "wss://relay.damus.io",
        "wss://nos.lol",
        "wss://relay.nostr.band",
        "wss://nostr.mom",
        "wss://relay.primal.net",
    ];

    for relay in &bootstrap {
        match fetch_nip65_write_relays_live(relay, pubkey).await {
            Ok(relays) if !relays.is_empty() => {
                println!("Found NIP-65 on {relay} ({} write relay(s)):", relays.len());
                for r in &relays {
                    println!("  {r}");
                }
                println!();
                return relays;
            }
            _ => {}
        }
    }

    println!("No NIP-65 relay list found anywhere.");
    println!("Using bootstrap relays as fallback:");
    let fallback: Vec<String> = bootstrap.iter().map(|s| s.to_string()).collect();
    for r in &fallback {
        println!("  {r}");
    }
    println!();
    fallback
}

async fn write_relays_from_db(pool: &sqlx::PgPool, pubkey: &str) -> Vec<String> {
    let row = sqlx::query_as::<_, (serde_json::Value,)>(
        "SELECT tags FROM events WHERE pubkey = $1 AND kind = 10002
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(pubkey)
    .fetch_optional(pool)
    .await
    .unwrap_or(None);

    match row {
        Some((tags,)) => parse_relay_tags_json(tags.as_array().unwrap_or(&vec![]))
            .into_iter()
            .filter(|(_, _, write)| *write)
            .map(|(url, _, _)| url)
            .collect(),
        None => vec![],
    }
}

/// Fetch kind-10002 for an author from a single relay and return write relay URLs.
async fn fetch_nip65_write_relays_live(
    relay_url: &str,
    pubkey: &str,
) -> Result<Vec<String>, String> {
    let (ws_stream, _) =
        timeout(Duration::from_secs(10), tokio_tungstenite::connect_async(relay_url))
            .await
            .map_err(|_| "connect timeout".to_string())?
            .map_err(|e| e.to_string())?;

    let (mut ws_write, mut ws_read) = ws_stream.split();

    let sub_id = format!("rl-{}", Uuid::new_v4().simple());
    let req = serde_json::json!(["REQ", &sub_id, {
        "kinds": [10002],
        "authors": [pubkey],
        "limit": 1
    }]);

    ws_write
        .send(Message::Text(req.to_string().into()))
        .await
        .map_err(|e| e.to_string())?;

    let mut result = vec![];

    loop {
        match timeout(Duration::from_secs(10), ws_read.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                let parsed: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let arr = match parsed.as_array() {
                    Some(a) if a.len() >= 3 => a,
                    _ => continue,
                };
                match arr[0].as_str() {
                    Some("EVENT") => {
                        if let Some(tags) = arr[2].get("tags").and_then(|t| t.as_array()) {
                            result = parse_relay_tags_json(tags)
                                .into_iter()
                                .filter(|(_, _, write)| *write)
                                .map(|(url, _, _)| url)
                                .collect();
                        }
                    }
                    Some("EOSE") | Some("CLOSED") => break,
                    _ => {}
                }
            }
            Ok(Some(Ok(Message::Ping(data)))) => {
                let _ = ws_write.send(Message::Pong(data)).await;
            }
            _ => break,
        }
    }

    Ok(result)
}

// ── Relay note counting (probe) ───────────────────────────────────────────────

/// Fetch all kind-1 event IDs for an author from a relay via paginated REQ.
/// Returns deduplicated event IDs. Shows inline progress for large authors.
async fn fetch_all_author_note_ids(
    relay_url: &str,
    pubkey: &str,
) -> Result<Vec<String>, String> {
    let page_size: usize = 500;

    let (ws_stream, _) =
        timeout(Duration::from_secs(10), tokio_tungstenite::connect_async(relay_url))
            .await
            .map_err(|_| "connect timeout".to_string())?
            .map_err(|e| format!("connect error: {e}"))?;

    let (mut ws_write, mut ws_read) = ws_stream.split();

    let mut all_ids: HashSet<String> = HashSet::new();
    let mut until: Option<i64> = None;
    let mut pages = 0usize;

    loop {
        let mut filter = serde_json::json!({
            "kinds": [1],
            "authors": [pubkey],
            "limit": page_size,
        });
        if let Some(u) = until {
            filter["until"] = serde_json::json!(u);
        }

        let sub_id = format!("probe-{}", Uuid::new_v4().simple());
        let req = serde_json::json!(["REQ", &sub_id, filter]);

        ws_write
            .send(Message::Text(req.to_string().into()))
            .await
            .map_err(|e| e.to_string())?;

        let mut page: Vec<(String, i64)> = Vec::new(); // (id, created_at)

        loop {
            match timeout(Duration::from_secs(20), ws_read.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    let parsed: Value = match serde_json::from_str(&text) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    let arr = match parsed.as_array() {
                        Some(a) if a.len() >= 2 => a,
                        _ => continue,
                    };
                    match arr[0].as_str() {
                        Some("EVENT") if arr.len() >= 3 => {
                            if let (Some(id), Some(ts)) = (
                                arr[2].get("id").and_then(|v| v.as_str()),
                                arr[2].get("created_at").and_then(|v| v.as_i64()),
                            ) {
                                page.push((id.to_string(), ts));
                            }
                        }
                        Some("EOSE") | Some("CLOSED") => break,
                        _ => {}
                    }
                }
                Ok(Some(Ok(Message::Ping(data)))) => {
                    let _ = ws_write.send(Message::Pong(data)).await;
                }
                Ok(Some(Ok(Message::Close(_)))) | Ok(None) => {
                    // Connection dropped mid-page; return what we have
                    for (id, _) in &page {
                        all_ids.insert(id.clone());
                    }
                    return Ok(all_ids.into_iter().collect());
                }
                _ => break,
            }
        }

        let close = serde_json::json!(["CLOSE", &sub_id]);
        let _ = ws_write.send(Message::Text(close.to_string().into())).await;

        let page_len = page.len();
        let oldest_ts = page.iter().map(|(_, ts)| *ts).min();
        for (id, _) in page {
            all_ids.insert(id);
        }
        pages += 1;

        // Show progress for large authors
        if pages > 1 {
            print!("\r    ... {} notes fetched so far", all_ids.len());
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }

        if page_len < page_size {
            // Received fewer than a full page — we've reached the end
            if pages > 1 {
                println!(); // newline after progress
            }
            break;
        }

        match oldest_ts {
            Some(ts) => until = Some(ts - 1),
            None => break,
        }
    }

    Ok(all_ids.into_iter().collect())
}

// ── REQ-based crawl (insert) ──────────────────────────────────────────────────

/// Paginated REQ crawl: fetch all kind-1 notes for an author and insert into DB.
/// Used when negentropy is unavailable or fails.
async fn crawl_via_paginated_req(
    relay_url: &str,
    pubkey: &str,
    repo: &nostr_api::db::repository::EventRepository,
    cache: &nostr_api::cache::StatsCache,
) -> usize {
    let page_size: usize = 500;

    let (ws_stream, _) =
        match timeout(Duration::from_secs(10), tokio_tungstenite::connect_async(relay_url)).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                println!("  connect failed: {e}");
                return 0;
            }
            Err(_) => {
                println!("  connect timeout");
                return 0;
            }
        };

    let (mut ws_write, mut ws_read) = ws_stream.split();

    let mut total_inserted = 0usize;
    let mut until: Option<i64> = None;
    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut pages = 0usize;

    loop {
        let mut filter = serde_json::json!({
            "kinds": [1],
            "authors": [pubkey],
            "limit": page_size,
        });
        if let Some(u) = until {
            filter["until"] = serde_json::json!(u);
        }

        let sub_id = format!("crawl-{}", Uuid::new_v4().simple());
        let req = serde_json::json!(["REQ", &sub_id, filter]);

        if ws_write
            .send(Message::Text(req.to_string().into()))
            .await
            .is_err()
        {
            break;
        }

        let mut page_events: Vec<nostr_api::db::models::NostrEvent> = Vec::new();

        loop {
            match timeout(Duration::from_secs(20), ws_read.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    let parsed: Value = match serde_json::from_str(&text) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    let arr = match parsed.as_array() {
                        Some(a) if a.len() >= 3 => a,
                        _ => continue,
                    };
                    match arr[0].as_str() {
                        Some("EVENT") => {
                            if let Ok(event) = serde_json::from_value::<
                                nostr_api::db::models::NostrEvent,
                            >(arr[2].clone())
                            {
                                if event.kind == 1 && !seen_ids.contains(&event.id) {
                                    seen_ids.insert(event.id.clone());
                                    page_events.push(event);
                                }
                            }
                        }
                        Some("EOSE") | Some("CLOSED") => break,
                        _ => {}
                    }
                }
                Ok(Some(Ok(Message::Ping(data)))) => {
                    let _ = ws_write.send(Message::Pong(data)).await;
                }
                _ => break,
            }
        }

        let close = serde_json::json!(["CLOSE", &sub_id]);
        let _ = ws_write.send(Message::Text(close.to_string().into())).await;

        let page_len = page_events.len();
        let oldest_ts = page_events.iter().map(|e| e.created_at).min();
        pages += 1;

        for event in &page_events {
            match repo.insert_event(event, relay_url).await {
                Ok(true) => {
                    total_inserted += 1;
                    cache.on_event_ingested(&event.pubkey, event.kind).await;
                }
                Ok(false) => {}
                Err(e) => eprintln!("  insert error: {e}"),
            }
        }

        if pages > 1 {
            println!(
                "  ... page {pages}: {} events, {total_inserted} inserted so far",
                page_len
            );
        }

        if page_len < page_size {
            break;
        }

        match oldest_ts {
            Some(ts) => until = Some(ts - 1),
            None => break,
        }
    }

    total_inserted
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Count how many of the given event IDs already exist in our DB.
async fn count_ids_in_db(pool: &sqlx::PgPool, ids: &[String]) -> usize {
    if ids.is_empty() {
        return 0;
    }
    let mut count = 0usize;
    for chunk in ids.chunks(1000) {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM events WHERE id = ANY($1)")
            .bind(chunk)
            .fetch_one(pool)
            .await
            .unwrap_or(0);
        count += n as usize;
    }
    count
}

/// Parse NIP-65 relay tags from a JSON array of tag arrays.
/// Returns `(url, read, write)` tuples.
fn parse_relay_tags_json(tags: &[serde_json::Value]) -> Vec<(String, bool, bool)> {
    let mut result = Vec::new();
    for tag in tags {
        let arr = match tag.as_array() {
            Some(a) => a,
            None => continue,
        };
        if arr.get(0).and_then(|v| v.as_str()) != Some("r") {
            continue;
        }
        let url = match arr.get(1).and_then(|v| v.as_str()) {
            Some(u) if !u.is_empty() => {
                u.trim().to_lowercase().trim_end_matches('/').to_string()
            }
            _ => continue,
        };
        if !url.starts_with("wss://") && !url.starts_with("ws://") {
            continue;
        }
        let (read, write) = match arr.get(2).and_then(|v| v.as_str()) {
            Some("read") => (true, false),
            Some("write") => (false, true),
            _ => (true, true),
        };
        result.push((url, read, write));
    }
    result
}

fn fmt_ts(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| ts.to_string())
}

/// Short display for a pubkey: first 8 + last 8 chars.
fn abbrev(pubkey: &str) -> String {
    if pubkey.len() >= 16 {
        format!("{}…{}", &pubkey[..8], &pubkey[pubkey.len() - 8..])
    } else {
        pubkey.to_string()
    }
}
