//! Integration test binary for negentropy sync and relay capability detection.
//!
//! Usage:
//!   cargo run --bin test_negentropy -- [command]
//!
//! Commands:
//!   probe <relay_url>       — Probe a relay for NIP-11 info and negentropy support
//!   discover                — Discover relays from stored NIP-65 data and probe top N
//!   sync <relay_url>        — Run a full negentropy sync against a relay
//!   dry-sync <relay_url>    — Reconcile only (no fetch/insert), report what would be fetched
//!   top-relays [limit]      — List top relays by NIP-65 user count
//!   relay-groups [limit]    — Show how crawl queue authors group by relay

use std::env;
use std::time::Instant;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter("nostr_api=debug,reqwest=warn")
        .init();

    let args: Vec<String> = env::args().collect();
    let command = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    let database_url = env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://dev:dev@localhost:5432/nostr_api".into());

    let pool = nostr_api::db::init_pool(&database_url)
        .await
        .expect("failed to connect to database");

    println!("Connected to database (migrations applied)");

    match command {
        "probe" => {
            let relay_url = args.get(2).expect("usage: test_negentropy probe <relay_url>");
            cmd_probe(&pool, relay_url).await;
        }
        "discover" => {
            let limit = args
                .get(2)
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(30);
            cmd_discover(&pool, limit).await;
        }
        "sync" => {
            let relay_url = args.get(2).expect("usage: test_negentropy sync <relay_url>");
            cmd_sync(&pool, relay_url, false).await;
        }
        "dry-sync" => {
            let relay_url = args
                .get(2)
                .expect("usage: test_negentropy dry-sync <relay_url>");
            cmd_sync(&pool, relay_url, true).await;
        }
        "windowed-sync" => {
            let relay_url = args
                .get(2)
                .expect("usage: test_negentropy windowed-sync <relay_url> [window_hours] [max_windows] [--dry-run]");
            let window_hours = args.get(3).and_then(|s| s.parse::<i64>().ok()).unwrap_or(24);
            let max_windows = args.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(7);
            let dry_run = args.iter().any(|a| a == "--dry-run");
            cmd_windowed_sync(&pool, relay_url, window_hours, max_windows, dry_run).await;
        }
        "top-relays" => {
            let limit = args
                .get(2)
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(30);
            cmd_top_relays(&pool, limit).await;
        }
        "relay-groups" => {
            let limit = args
                .get(2)
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(20);
            cmd_relay_groups(&pool, limit).await;
        }
        _ => {
            println!("negentropy test tool");
            println!();
            println!("Commands:");
            println!("  probe <relay_url>            Probe relay for NIP-11 and negentropy support");
            println!("  discover [limit]             Discover top relays from NIP-65 data and probe each");
            println!("  sync <relay_url>             Full negentropy sync (reconcile + fetch + insert)");
            println!("  dry-sync <relay_url>         Reconcile only, report what would be fetched");
            println!("  windowed-sync <relay_url> [window_hours] [max_windows] [--dry-run]");
            println!("                               Sync using backward time windows (default: 24h x 7)");
            println!("  top-relays [limit]           List top relays by NIP-65 user count");
            println!("  relay-groups [limit]         Show crawl queue author grouping by relay");
            println!();
            println!("Set DATABASE_URL to point to your database.");
        }
    }
}

async fn cmd_probe(pool: &sqlx::PgPool, relay_url: &str) {
    use nostr_api::crawler::relay_caps;

    println!("\n=== Probing {relay_url} ===\n");

    // NIP-11
    println!("--- NIP-11 probe ---");
    match relay_caps::probe_nip11(relay_url).await {
        Ok(caps) => {
            println!("  supports_negentropy: {}", caps.supports_negentropy);
            println!("  max_limit: {:?}", caps.max_limit);
            if let Some(ref nip11) = caps.nip11 {
                if let Some(name) = nip11.get("name").and_then(|v| v.as_str()) {
                    println!("  relay name: {name}");
                }
                if let Some(sw) = nip11.get("software").and_then(|v| v.as_str()) {
                    println!("  software: {sw}");
                }
                if let Some(nips) = nip11.get("supported_nips").and_then(|v| v.as_array()) {
                    let nip_nums: Vec<String> = nips.iter().map(|n| n.to_string()).collect();
                    println!("  supported_nips: [{}]", nip_nums.join(", "));
                }
            }
        }
        Err(e) => {
            println!("  NIP-11 failed: {e}");
        }
    }

    // NEG-OPEN probe
    println!("\n--- NEG-OPEN probe ---");
    match relay_caps::probe_neg_open(relay_url).await {
        Ok(true) => println!("  NEG-OPEN: SUPPORTED ✓"),
        Ok(false) => println!("  NEG-OPEN: NOT SUPPORTED ✗"),
        Err(e) => println!("  NEG-OPEN probe failed: {e}"),
    }

    // Full check_and_update (tests DB caching)
    println!("\n--- Full capability check (with DB cache) ---");
    match relay_caps::check_and_update_caps(pool, relay_url).await {
        Ok(caps) => {
            println!("  supports_negentropy: {}", caps.supports_negentropy);
            println!("  max_limit: {:?}", caps.max_limit);
            println!("  last_checked: {}", caps.last_checked_at);
            println!("  (saved to relay_capabilities table)");
        }
        Err(e) => {
            println!("  check_and_update failed: {e}");
        }
    }
}

async fn cmd_discover(pool: &sqlx::PgPool, limit: i64) {
    use nostr_api::crawler::relay_caps;

    println!("\n=== Discovering relays from NIP-65 data ===\n");

    let relays = match relay_caps::discover_relays_from_nip65(pool).await {
        Ok(r) => r,
        Err(e) => {
            println!("Failed to discover relays: {e}");
            return;
        }
    };

    println!("Found {} unique relays in NIP-65 data\n", relays.len());

    let top = &relays[..relays.len().min(limit as usize)];
    println!("Probing top {} relays...\n", top.len());

    let mut neg_capable = 0;
    for (i, (url, user_count)) in top.iter().enumerate() {
        print!("[{}/{}] {} ({} users) ... ", i + 1, top.len(), url, user_count);

        // Quick NIP-11 check first
        let supports = match relay_caps::probe_nip11(url).await {
            Ok(caps) => {
                if caps.supports_negentropy {
                    true
                } else {
                    // Fallback to NEG-OPEN probe
                    relay_caps::probe_neg_open(url).await.unwrap_or(false)
                }
            }
            Err(_) => relay_caps::probe_neg_open(url).await.unwrap_or(false),
        };

        if supports {
            println!("NEGENTROPY ✓");
            neg_capable += 1;
        } else {
            println!("no negentropy");
        }

        // Save to DB
        let caps = nostr_api::crawler::relay_caps::RelayCaps {
            relay_url: url.clone(),
            supports_negentropy: supports,
            max_limit: None,
            nip11: None,
            last_checked_at: chrono::Utc::now(),
        };
        let _ = relay_caps::upsert_relay_caps(pool, &caps).await;
    }

    println!("\n=== Results ===");
    println!("  Relays probed: {}", top.len());
    println!("  Negentropy capable: {neg_capable}");
    println!(
        "  Coverage: {:.1}%",
        (neg_capable as f64 / top.len() as f64) * 100.0
    );
}

async fn cmd_sync(pool: &sqlx::PgPool, relay_url: &str, dry_run: bool) {
    use nostr_api::crawler::negentropy::NegentropySyncer;

    let redis_url = env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());
    let redis_client = redis::Client::open(redis_url.as_str()).expect("invalid redis url");
    
    // Create a dummy follower cache for testing
    let follower_cache = nostr_api::follower_cache::FollowerCache::new(pool.clone(), 5, 3600);
    let wot_cache = nostr_api::wot_cache::WotCache::new(pool.clone(), 21, 900);
    let block_cache = nostr_api::block_cache::BlockCache::new(pool.clone());
    block_cache.initialize().await.expect("failed to initialize block cache");
    let repo = nostr_api::db::repository::EventRepository::new(pool.clone(), follower_cache, wot_cache, block_cache);
    let cache = nostr_api::cache::StatsCache::new(redis_client, repo.clone());
    let syncer = NegentropySyncer::new(repo, cache, pool.clone());

    // Use a 24h window by default to avoid "too many results" errors
    let now = chrono::Utc::now().timestamp();
    let since = now - 86400;

    let start = Instant::now();

    if dry_run {
        println!("\n=== Dry-run negentropy reconciliation with {relay_url} ===");
        println!("  Window: last 24 hours\n");
        println!("Reconciling (discovering diff, NOT fetching events)...\n");

        match syncer
            .sync_with_relay_window(relay_url, &[1], Some(since), Some(now))
            .await
        {
            Ok(result) => {
                let elapsed = start.elapsed();
                println!("=== Reconciliation complete ===");
                println!("  Duration: {:.2}s", elapsed.as_secs_f64());
                println!(
                    "  Events we have, relay doesn't: {}",
                    result.have_ids.len()
                );
                println!(
                    "  Events relay has, we don't: {}",
                    result.need_ids.len()
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
    } else {
        println!("\n=== Full negentropy sync with {relay_url} ===");
        println!("  Window: last 24 hours");
        println!("  This will reconcile, fetch, and INSERT events into the database.\n");

        match syncer
            .run_sync_window(relay_url, &[1], Some(since), Some(now))
            .await
        {
            Ok(stats) => {
                println!("=== Sync complete ===");
                println!("  Duration: {}ms", stats.duration_ms);
                println!("  Events discovered: {}", stats.events_discovered);
                println!("  Events fetched: {}", stats.events_fetched);
                println!("  Events inserted: {}", stats.events_inserted);
            }
            Err(e) => {
                println!("Sync failed: {e}");
            }
        }
    }
}

async fn cmd_windowed_sync(
    pool: &sqlx::PgPool,
    relay_url: &str,
    window_hours: i64,
    max_windows: usize,
    dry_run: bool,
) {
    use nostr_api::crawler::negentropy::NegentropySyncer;

    let redis_url = env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());
    let redis_client = redis::Client::open(redis_url.as_str()).expect("invalid redis url");
    
    // Create a dummy follower cache for testing
    let follower_cache = nostr_api::follower_cache::FollowerCache::new(pool.clone(), 5, 3600);
    let wot_cache = nostr_api::wot_cache::WotCache::new(pool.clone(), 21, 900);
    let block_cache = nostr_api::block_cache::BlockCache::new(pool.clone());
    block_cache.initialize().await.expect("failed to initialize block cache");
    let repo = nostr_api::db::repository::EventRepository::new(pool.clone(), follower_cache, wot_cache, block_cache);
    let cache = nostr_api::cache::StatsCache::new(redis_client, repo.clone());
    let syncer = NegentropySyncer::new(repo, cache, pool.clone());

    let window_secs = window_hours * 3600;

    println!("\n=== Windowed negentropy sync with {relay_url} ===");
    println!("  Window size: {}h", window_hours);
    println!("  Max windows: {max_windows}");
    println!("  Dry run: {dry_run}");

    if dry_run {
        println!("\n  DRY RUN — reconciling windows but NOT fetching/inserting\n");

        let now = chrono::Utc::now().timestamp();
        let mut cursor = now;
        let mut total_need = 0usize;
        let mut total_have = 0usize;
        let mut window = window_secs;
        let min_window: i64 = 3600;

        for i in 0..max_windows {
            let w_since = cursor - window;
            let w_until = cursor;

            print!(
                "  Window {} ({} → {}): ",
                i + 1,
                chrono::DateTime::from_timestamp(w_since, 0)
                    .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_default(),
                chrono::DateTime::from_timestamp(w_until, 0)
                    .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_default(),
            );

            match syncer
                .sync_with_relay_window(relay_url, &[1], Some(w_since), Some(w_until))
                .await
            {
                Ok(result) => {
                    println!(
                        "need {} / have {}",
                        result.need_ids.len(),
                        result.have_ids.len()
                    );
                    total_need += result.need_ids.len();
                    total_have += result.have_ids.len();
                    cursor = w_since;
                    if result.need_ids.is_empty() && i > 0 {
                        println!("  Empty window — stopping.\n");
                        break;
                    }
                }
                Err(e) => {
                    let msg = format!("{e}");
                    if msg.contains("too many") || msg.contains("blocked") {
                        let new_window = window / 2;
                        if new_window < min_window {
                            println!("BLOCKED (window too small, skipping)");
                            cursor = w_since;
                            continue;
                        }
                        println!("BLOCKED — halving window to {}h", new_window / 3600);
                        window = new_window;
                        continue; // retry same cursor
                    }
                    println!("ERROR: {e}");
                    cursor = w_since;
                }
            }
        }

        println!("=== Dry-run complete ===");
        println!("  Total events relay has, we don't: {total_need}");
        println!("  Total events we have, relay doesn't: {total_have}");
    } else {
        println!("\n  LIVE — will fetch and INSERT events\n");

        match syncer
            .run_sync_windowed(relay_url, Some(window_secs), max_windows)
            .await
        {
            Ok(stats) => {
                println!("=== Windowed sync complete ===");
                println!("  Duration: {}ms", stats.duration_ms);
                println!("  Events discovered: {}", stats.events_discovered);
                println!("  Events fetched: {}", stats.events_fetched);
                println!("  Events inserted: {}", stats.events_inserted);
            }
            Err(e) => {
                println!("Windowed sync failed: {e}");
            }
        }
    }
}

async fn cmd_top_relays(pool: &sqlx::PgPool, limit: i64) {
    use nostr_api::crawler::relay_router::RelayRouter;

    let router = RelayRouter::new(pool.clone());
    println!("\n=== Top relays by NIP-65 user count ===\n");

    match router.get_top_relays(limit).await {
        Ok(relays) => {
            if relays.is_empty() {
                println!("No kind-10002 relay list data found.");
                return;
            }
            println!("{:<50} {:>8}", "RELAY", "USERS");
            println!("{}", "-".repeat(60));
            for (url, count) in &relays {
                println!("{:<50} {:>8}", url, count);
            }
            println!("\nTotal: {} relays", relays.len());
        }
        Err(e) => {
            println!("Failed: {e}");
        }
    }
}

async fn cmd_relay_groups(pool: &sqlx::PgPool, limit: i64) {
    use nostr_api::crawler::queue::CrawlQueue;
    use nostr_api::crawler::relay_router::RelayRouter;

    let queue = CrawlQueue::new(pool.clone());
    let router = RelayRouter::new(pool.clone());

    println!("\n=== Relay grouping for crawl queue ===\n");

    // Peek at next batch without locking (just query)
    let targets = match sqlx::query_as::<_, (String,)>(
        "SELECT pubkey FROM crawl_state WHERE next_crawl_at <= NOW() ORDER BY priority_tier ASC, follower_count DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows.into_iter().map(|r| r.0).collect::<Vec<_>>(),
        Err(e) => {
            println!("Failed to query crawl queue: {e}");
            return;
        }
    };

    if targets.is_empty() {
        println!("No authors ready to crawl.");
        return;
    }

    println!("Checking relay preferences for {} authors...\n", targets.len());

    match router.get_relay_author_groups(&targets).await {
        Ok(groups) => {
            let mut sorted: Vec<_> = groups.iter().collect();
            sorted.sort_by(|a, b| b.1.len().cmp(&a.1.len()));

            let total_routed: usize = groups.values().map(|v| v.len()).sum();
            let unique_routed: std::collections::HashSet<&String> =
                groups.values().flat_map(|v| v.iter()).collect();

            println!("{:<50} {:>8}", "RELAY", "AUTHORS");
            println!("{}", "-".repeat(60));
            for (url, authors) in sorted.iter().take(30) {
                println!("{:<50} {:>8}", url, authors.len());
            }
            if sorted.len() > 30 {
                println!("  ... and {} more relays", sorted.len() - 30);
            }

            let unrouted = targets.len() - unique_routed.len();
            println!("\n--- Summary ---");
            println!("  Authors checked: {}", targets.len());
            println!("  Authors with relay prefs: {}", unique_routed.len());
            println!("  Authors without (need fallback): {unrouted}");
            println!("  Unique relays: {}", groups.len());
            println!("  Total author-relay mappings: {total_routed}");
        }
        Err(e) => {
            println!("Failed: {e}");
        }
    }

    // Also show queue stats
    match queue.stats().await {
        Ok(stats) => {
            println!("\n--- Queue stats ---");
            println!("  Total authors: {}", stats.total_authors);
            println!("  Ready to crawl: {}", stats.ready_to_crawl);
            println!("  Already crawled: {}", stats.authors_crawled);
            println!(
                "  Notes crawled: {}",
                stats.total_notes_crawled
            );
        }
        Err(e) => println!("  Queue stats failed: {e}"),
    }
}
