//! CLI tool to purge kind-1 notes from authors who don't pass the WoT filter.
//!
//! Strategy: delete one author's notes at a time with a short sleep between
//! each author. This keeps transactions small and avoids lock contention with
//! the live service. Safe to run in the background.
//!
//! Usage:
//!   cargo run --bin purge_non_wot                       # dry run
//!   cargo run --bin purge_non_wot -- --execute           # delete (50ms sleep)
//!   cargo run --bin purge_non_wot -- --execute --sleep 0 # no sleep (faster)

use std::time::Instant;

use sqlx::postgres::PgPoolOptions;
use tokio::time::{sleep, Duration};

const DEFAULT_SLEEP_MS: u64 = 50;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    let execute = args.iter().any(|a| a == "--execute");
    let sleep_ms = args
        .iter()
        .position(|a| a == "--sleep")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_SLEEP_MS);

    let database_url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");

    let pool = PgPoolOptions::new()
        .max_connections(3)
        .acquire_timeout(std::time::Duration::from_secs(30))
        .connect(&database_url)
        .await?;

    // Step 1: Safety check
    let (wot_count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM wot_scores WHERE passes_wot = true")
            .fetch_one(&pool)
            .await?;
    println!("WoT-passing pubkeys: {wot_count}");

    if wot_count == 0 {
        println!("ERROR: wot_scores is empty — refusing to purge");
        return Ok(());
    }

    // Step 2: Get list of non-WoT pubkeys that have kind-1 events
    println!("Identifying non-WoT authors with kind-1 notes...");
    let build_start = Instant::now();

    let pubkeys: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT e.pubkey
         FROM events e
         LEFT JOIN wot_scores w ON w.pubkey = e.pubkey AND w.passes_wot = true
         WHERE e.kind = 1 AND w.pubkey IS NULL",
    )
    .fetch_all(&pool)
    .await?;

    let purge_authors = pubkeys.len();
    println!(
        "Found {purge_authors} non-WoT authors in {:.1}s",
        build_start.elapsed().as_secs_f64()
    );

    // Step 3: Count
    let (total_kind1,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM events WHERE kind = 1")
            .fetch_one(&pool)
            .await?;

    let (keep_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM events e
         INNER JOIN wot_scores w ON w.pubkey = e.pubkey AND w.passes_wot = true
         WHERE e.kind = 1",
    )
    .fetch_one(&pool)
    .await?;

    let purge_count = total_kind1 - keep_count;

    println!("\n--- Purge Summary ---");
    println!("Total kind-1 notes:     {total_kind1}");
    println!("Notes to KEEP (in WoT): {keep_count}");
    println!("Notes to PURGE:         {purge_count}");
    println!("Authors to purge:       {purge_authors}");
    println!(
        "Purge percentage:       {:.1}%",
        if total_kind1 > 0 {
            100.0 * purge_count as f64 / total_kind1 as f64
        } else {
            0.0
        }
    );

    if purge_count == 0 {
        println!("\nNothing to purge!");
        return Ok(());
    }

    if !execute {
        println!("\n** DRY RUN — pass --execute to actually delete **");
        return Ok(());
    }

    // === EXECUTE: delete per-author ===
    println!("\nPurging one author at a time ({sleep_ms}ms sleep between)...\n");

    let start = Instant::now();
    let mut total_deleted: u64 = 0;
    let mut authors_done: usize = 0;

    for (pubkey,) in &pubkeys {
        let result = sqlx::query(
            "DELETE FROM events WHERE pubkey = $1 AND kind = 1",
        )
        .bind(pubkey)
        .execute(&pool)
        .await?;

        let deleted = result.rows_affected();
        total_deleted += deleted;
        authors_done += 1;

        if authors_done % 500 == 0 || authors_done == purge_authors {
            let elapsed = start.elapsed().as_secs();
            let rate = if elapsed > 0 { total_deleted / elapsed } else { total_deleted };
            let pct = 100.0 * authors_done as f64 / purge_authors as f64;
            println!(
                "  authors {authors_done}/{purge_authors} ({pct:.1}%) — {total_deleted} events deleted — {rate} events/sec"
            );
        }

        if sleep_ms > 0 {
            sleep(Duration::from_millis(sleep_ms)).await;
        }
    }

    let elapsed = start.elapsed();
    println!(
        "\nDone! Deleted {total_deleted} events from {authors_done} authors in {:.1}s",
        elapsed.as_secs_f64()
    );

    // Step 5: Validate NOT VALID FK constraints if any exist
    println!("\nChecking FK constraint validity...");
    let not_valid: Vec<(String, String)> = sqlx::query_as(
        "SELECT tc.table_name::text, tc.constraint_name::text
         FROM information_schema.table_constraints tc
         JOIN pg_constraint c ON c.conname = tc.constraint_name
         WHERE tc.constraint_type = 'FOREIGN KEY'
           AND NOT c.convalidated
           AND tc.table_name IN ('event_refs', 'follows', 'follow_lists')",
    )
    .fetch_all(&pool)
    .await?;

    if not_valid.is_empty() {
        println!("  all FK constraints are valid");
    } else {
        for (table, constraint) in &not_valid {
            println!("  validating {table}.{constraint}...");
            let t = Instant::now();
            let sql = format!("ALTER TABLE {table} VALIDATE CONSTRAINT {constraint}");
            sqlx::query(&sql).execute(&pool).await?;
            println!("    validated in {:.1}s", t.elapsed().as_secs_f64());
        }
    }

    println!("\nAll clean. Run VACUUM events; when convenient to reclaim space.");

    Ok(())
}
