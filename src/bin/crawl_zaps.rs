//! CLI tool for negentropy-based zap receipt (kind 9735) crawling.
//!
//! Performs bulk windowed negentropy sync for zap receipts across all
//! negentropy-capable relays. No WoT filtering — fetches ALL zaps.
//! Uses persistent cursors (negentropy_sync_state table) so it can
//! be stopped and resumed.
//!
//! Usage:
//!   cargo run --bin crawl_zaps -- [command] [options]
//!
//! Commands:
//!   crawl [--relays url1,url2] [--max-windows N] [--window-hours N]
//!       Run full zap negentropy crawl with persistent cursors.
//!       Default relays: NEGENTROPY_PINNED_RELAYS from env/config.
//!       Walks backward from now to Nostr epoch per relay.
//!
//!   status
//!       Show sync state for kind 9735 across all relays.
//!
//!   dry-run [relay_url] [--window-hours N]
//!       Reconcile only against a single relay, report what would be fetched.
//!
//!   probe [--relays url1,url2]
//!       Probe relays for negentropy support.
//!
//! Environment:
//!   DATABASE_URL          PostgreSQL connection string
//!   REDIS_URL             Redis connection string
//!   NEGENTROPY_PINNED_RELAYS   Comma-separated relay URLs (default: 7 relays)

use std::env;
use std::time::Instant;

use nostr_api::cache::StatsCache;
use nostr_api::crawler::negentropy::NegentropySyncer;
use nostr_api::crawler::relay_caps;

const KIND_ZAP: i64 = 9735;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter("nostr_api=info,reqwest=warn")
        .init();

    let args: Vec<String> = env::args().collect();
    let command = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    let database_url = env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://dev:dev@localhost:5432/nostr_api".into());

    let pool = nostr_api::db::init_pool(&database_url)
        .await
        .expect("failed to connect to database");

    println!("Connected to database");

    match command {
        "crawl" => {
            let relays = parse_relays_arg(&args);
            let max_windows = parse_named_arg(&args, "--max-windows")
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(usize::MAX);
            let window_hours = parse_named_arg(&args, "--window-hours")
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(24);
            cmd_crawl(&pool, relays, max_windows, window_hours).await;
        }
        "status" => {
            cmd_status(&pool).await;
        }
        "dry-run" => {
            let relay_url = args.get(2).expect("usage: crawl_zaps dry-run <relay_url> [--window-hours N]");
            let window_hours = parse_named_arg(&args, "--window-hours")
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(24);
            cmd_dry_run(&pool, relay_url, window_hours).await;
        }
        "probe" => {
            let relays = parse_relays_arg(&args);
            cmd_probe(relays).await;
        }
        _ => {
            println!("crawl_zaps — negentropy zap receipt crawler");
            println!();
            println!("Commands:");
            println!("  crawl [--relays url1,url2] [--max-windows N] [--window-hours N]");
            println!("      Full zap crawl with persistent cursors across all relays.");
            println!("      Walks backward from now to Nostr epoch. Resumable.");
            println!();
            println!("  status");
            println!("      Show kind-9735 sync state for all relays.");
            println!();
            println!("  dry-run <relay_url> [--window-hours N]");
            println!("      Reconcile only, report what would be fetched (default: 24h window).");
            println!();
            println!("  probe [--relays url1,url2]");
            println!("      Probe relays for negentropy support.");
            println!();
            println!("Set DATABASE_URL and REDIS_URL in env or .env file.");
            println!("Override relays with --relays or NEGENTROPY_PINNED_RELAYS env.");
        }
    }
}

fn default_relays() -> Vec<String> {
    env::var("NEGENTROPY_PINNED_RELAYS")
        .unwrap_or_else(|_| {
            [
                "wss://relay.damus.io",
                "wss://nos.lol",
                "wss://relay.primal.net",
                "wss://relay.nostr.band",
                "wss://nostr.wine",
                "wss://purplepag.es",
                "wss://relay.nos.social",
            ]
            .join(",")
        })
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn parse_relays_arg(args: &[String]) -> Vec<String> {
    parse_named_arg(args, "--relays")
        .map(|s| {
            s.split(',')
                .map(|r| r.trim().to_string())
                .filter(|r| !r.is_empty())
                .collect()
        })
        .unwrap_or_else(default_relays)
}

fn parse_named_arg<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
}

async fn build_syncer(pool: &sqlx::PgPool) -> NegentropySyncer {
    let redis_url = env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());
    let redis_client = redis::Client::open(redis_url.as_str()).expect("invalid redis url");

    let follower_cache =
        nostr_api::follower_cache::FollowerCache::new(pool.clone(), 5, 3600);
    let wot_cache = nostr_api::wot_cache::WotCache::new(pool.clone(), 21, 900);
    let block_cache = nostr_api::block_cache::BlockCache::new(pool.clone());
    block_cache.initialize().await.expect("failed to initialize block cache");
    let repo = nostr_api::db::repository::EventRepository::new(
        pool.clone(),
        follower_cache,
        wot_cache,
        block_cache,
    );
    let cache = StatsCache::new(redis_client, repo.clone());
    NegentropySyncer::new(repo, cache, pool.clone())
}

async fn validate_relays(relays: &[String]) -> Vec<String> {
    println!("\nProbing {} relays for negentropy support...\n", relays.len());

    let mut confirmed = Vec::new();
    for relay_url in relays {
        print!("  {} ... ", relay_url);
        match relay_caps::probe_neg_open(relay_url).await {
            Ok(true) => {
                println!("OK");
                confirmed.push(relay_url.clone());
            }
            Ok(false) => {
                println!("no negentropy");
            }
            Err(e) => {
                println!("error: {e}");
            }
        }
    }

    println!(
        "\n{}/{} relays confirmed negentropy support\n",
        confirmed.len(),
        relays.len()
    );

    confirmed
}

async fn cmd_crawl(pool: &sqlx::PgPool, relays: Vec<String>, max_windows: usize, window_hours: i64) {
    let confirmed = validate_relays(&relays).await;
    if confirmed.is_empty() {
        println!("No negentropy-capable relays found. Aborting.");
        return;
    }

    let syncer = build_syncer(pool).await;
    let now = chrono::Utc::now().timestamp();
    let initial_window_secs = window_hours * 3600;
    let kinds = &[KIND_ZAP];

    let grand_start = Instant::now();
    let mut grand_discovered = 0usize;
    let mut grand_fetched = 0usize;
    let mut grand_inserted = 0usize;

    // Count existing zaps for context
    let existing_zaps: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM events WHERE kind = 9735")
            .fetch_one(pool)
            .await
            .unwrap_or(0);
    println!("Existing kind-9735 events in DB: {}\n", existing_zaps);

    for relay_url in &confirmed {
        println!("=== {} ===\n", relay_url);

        // Load persisted sync state for this relay + kind 9735
        let state = match syncer.get_sync_state(relay_url, KIND_ZAP).await {
            Ok(s) => s,
            Err(e) => {
                println!("  Failed to load sync state: {e}");
                continue;
            }
        };

        // --- Recent pass: always sync last 24h ---
        println!("  Recent pass (last 24h)...");
        let recent_since = now - NegentropySyncer::DEFAULT_WINDOW_SECS;
        match syncer
            .run_sync_window(relay_url, kinds, Some(recent_since), Some(now))
            .await
        {
            Ok(stats) => {
                println!(
                    "    discovered={} fetched={} inserted={} ({}ms)",
                    stats.events_discovered,
                    stats.events_fetched,
                    stats.events_inserted,
                    stats.duration_ms
                );
                grand_discovered += stats.events_discovered;
                grand_fetched += stats.events_fetched;
                grand_inserted += stats.events_inserted;

                if now > state.newest_synced_at {
                    let _ = syncer
                        .update_sync_state(relay_url, KIND_ZAP, |s| {
                            s.newest_synced_at = now;
                        })
                        .await;
                }
            }
            Err(e) => {
                println!("    recent pass failed: {e}");
            }
        }

        // --- Backfill pass: walk backward from cursor ---
        if state.fully_backfilled {
            println!("  Already fully backfilled, skipping backfill pass.\n");
            continue;
        }

        let backfill_cursor = if state.oldest_synced_at > 0 {
            state.oldest_synced_at
        } else {
            recent_since
        };

        let resume_window = if state.current_window_secs > 0 {
            state.current_window_secs.max(initial_window_secs)
        } else {
            initial_window_secs
        };

        let cursor_date = chrono::DateTime::from_timestamp(backfill_cursor, 0)
            .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "unknown".into());

        println!(
            "  Backfill pass from {} (window: {}h, max_windows: {})...",
            cursor_date,
            resume_window / 3600,
            if max_windows == usize::MAX {
                "unlimited".to_string()
            } else {
                max_windows.to_string()
            }
        );

        match syncer
            .run_sync_windowed_from(relay_url, kinds, backfill_cursor, resume_window, max_windows)
            .await
        {
            Ok((stats, final_window, final_cursor)) => {
                grand_discovered += stats.events_discovered;
                grand_fetched += stats.events_fetched;
                grand_inserted += stats.events_inserted;

                let fully_done = final_cursor <= NegentropySyncer::NOSTR_EPOCH;

                let _ = syncer
                    .update_sync_state(relay_url, KIND_ZAP, |s| {
                        s.oldest_synced_at = final_cursor.max(0);
                        s.total_discovered += stats.events_discovered as i64;
                        s.total_inserted += stats.events_inserted as i64;
                        s.current_window_secs = final_window;
                        s.consecutive_empty_windows = 0;
                        s.fully_backfilled = fully_done;
                    })
                    .await;

                let cursor_date = chrono::DateTime::from_timestamp(final_cursor, 0)
                    .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_else(|| "unknown".into());

                println!(
                    "    discovered={} fetched={} inserted={} ({}ms)",
                    stats.events_discovered,
                    stats.events_fetched,
                    stats.events_inserted,
                    stats.duration_ms
                );
                println!(
                    "    cursor reached: {} | window: {}h | {}",
                    cursor_date,
                    final_window / 3600,
                    if fully_done {
                        "FULLY BACKFILLED"
                    } else {
                        "more to go (re-run to continue)"
                    }
                );
            }
            Err(e) => {
                println!("    backfill failed: {e}");
            }
        }

        println!();
    }

    // Final count
    let final_zaps: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM events WHERE kind = 9735")
            .fetch_one(pool)
            .await
            .unwrap_or(0);

    let elapsed = grand_start.elapsed();
    println!("=== Summary ===");
    println!("  Relays synced:   {}", confirmed.len());
    println!("  Discovered:      {}", grand_discovered);
    println!("  Fetched:         {}", grand_fetched);
    println!("  Inserted:        {}", grand_inserted);
    println!("  Duration:        {:.1}s", elapsed.as_secs_f64());
    println!("  Zaps before:     {}", existing_zaps);
    println!("  Zaps after:      {}", final_zaps);
    println!("  Net new zaps:    {}", final_zaps - existing_zaps);
}

async fn cmd_status(pool: &sqlx::PgPool) {
    println!("\n=== Kind-9735 (zap) sync state ===\n");

    let rows = sqlx::query_as::<_, SyncStateDisplay>(
        "SELECT relay_url, oldest_synced_at, newest_synced_at, fully_backfilled, \
                total_discovered, total_inserted, current_window_secs, last_sync_at \
         FROM negentropy_sync_state WHERE kind = $1 ORDER BY relay_url",
    )
    .bind(KIND_ZAP)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    if rows.is_empty() {
        println!("No sync state found for kind 9735. Run 'crawl' first.");
        return;
    }

    for row in &rows {
        let oldest = chrono::DateTime::from_timestamp(row.oldest_synced_at, 0)
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "-".into());
        let newest = chrono::DateTime::from_timestamp(row.newest_synced_at, 0)
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "-".into());
        let last_sync = row
            .last_sync_at
            .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "-".into());

        println!("{}", row.relay_url);
        println!(
            "  range: {} -> {} | backfilled: {} | window: {}h",
            oldest,
            newest,
            if row.fully_backfilled { "yes" } else { "no" },
            row.current_window_secs / 3600,
        );
        println!(
            "  discovered: {} | inserted: {} | last sync: {}",
            row.total_discovered, row.total_inserted, last_sync,
        );
        println!();
    }

    // Overall zap count
    let total_zaps: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM events WHERE kind = 9735")
            .fetch_one(pool)
            .await
            .unwrap_or(0);
    let total_zap_metadata: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM zap_metadata")
            .fetch_one(pool)
            .await
            .unwrap_or(0);

    println!("--- Totals ---");
    println!("  Kind-9735 events: {}", total_zaps);
    println!("  Zap metadata rows: {}", total_zap_metadata);
}

async fn cmd_dry_run(pool: &sqlx::PgPool, relay_url: &str, window_hours: i64) {
    let syncer = build_syncer(pool).await;
    let now = chrono::Utc::now().timestamp();
    let since = now - (window_hours * 3600);

    println!(
        "\n=== Dry-run: kind-9735 negentropy reconciliation with {} ===",
        relay_url
    );
    println!("  Window: last {}h\n", window_hours);

    let start = Instant::now();
    match syncer
        .sync_with_relay_window(relay_url, &[KIND_ZAP], Some(since), Some(now))
        .await
    {
        Ok(result) => {
            let elapsed = start.elapsed();
            println!("Reconciliation complete ({:.2}s)", elapsed.as_secs_f64());
            println!(
                "  Events relay has, we don't: {}",
                result.need_ids.len()
            );
            println!(
                "  Events we have, relay doesn't: {}",
                result.have_ids.len()
            );
            if !result.need_ids.is_empty() {
                println!("\n  First 10 needed IDs:");
                for id in result.need_ids.iter().take(10) {
                    println!("    {id}");
                }
            }
        }
        Err(e) => {
            println!("Reconciliation failed: {e}");
        }
    }
}

async fn cmd_probe(relays: Vec<String>) {
    validate_relays(&relays).await;
}

#[derive(sqlx::FromRow)]
struct SyncStateDisplay {
    relay_url: String,
    oldest_synced_at: i64,
    newest_synced_at: i64,
    fully_backfilled: bool,
    total_discovered: i64,
    total_inserted: i64,
    current_window_secs: i64,
    last_sync_at: Option<chrono::DateTime<chrono::Utc>>,
}
