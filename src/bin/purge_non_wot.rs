//! CLI tool to purge kind-1 notes from authors who don't pass the WoT filter.
//!
//! Strategy: materialize non-WoT pubkeys into a temp table once, then batch-delete
//! using an indexed join — avoids the expensive NOT IN subquery on every batch.
//!
//! Usage:
//!   cargo run --bin purge_non_wot                  # dry run (default)
//!   cargo run --bin purge_non_wot -- --execute      # actually delete
//!   cargo run --bin purge_non_wot -- --batch 5000   # custom batch size

use std::time::Instant;

use sqlx::postgres::PgPoolOptions;

const DEFAULT_BATCH_SIZE: i64 = 10_000;

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
    let batch_size = args
        .iter()
        .position(|a| a == "--batch")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(DEFAULT_BATCH_SIZE);

    let database_url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(std::time::Duration::from_secs(30))
        .connect(&database_url)
        .await?;

    // Step 1: Count WoT-passing pubkeys
    let (wot_count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM wot_scores WHERE passes_wot = true")
            .fetch_one(&pool)
            .await?;
    println!("WoT-passing pubkeys: {wot_count}");

    if wot_count == 0 {
        println!("ERROR: wot_scores is empty — refusing to purge (would delete everything)");
        return Ok(());
    }

    // Step 2: Build temp table of non-WoT pubkeys that have kind-1 events
    println!("\nIdentifying non-WoT authors with kind-1 notes...");
    let build_start = Instant::now();

    sqlx::query("DROP TABLE IF EXISTS _purge_pubkeys")
        .execute(&pool)
        .await?;

    sqlx::query(
        "CREATE UNLOGGED TABLE _purge_pubkeys AS
         SELECT DISTINCT e.pubkey
         FROM events e
         LEFT JOIN wot_scores w ON w.pubkey = e.pubkey AND w.passes_wot = true
         WHERE e.kind = 1 AND w.pubkey IS NULL",
    )
    .execute(&pool)
    .await?;

    sqlx::query("CREATE INDEX ON _purge_pubkeys (pubkey)")
        .execute(&pool)
        .await?;

    let (purge_authors,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM _purge_pubkeys")
            .fetch_one(&pool)
            .await?;

    println!(
        "Found {purge_authors} non-WoT authors in {:.1}s",
        build_start.elapsed().as_secs_f64()
    );

    // Step 3: Count events to purge
    let (total_kind1,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM events WHERE kind = 1")
            .fetch_one(&pool)
            .await?;

    let (purge_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM events e
         INNER JOIN _purge_pubkeys p ON p.pubkey = e.pubkey
         WHERE e.kind = 1",
    )
    .fetch_one(&pool)
    .await?;

    let keep_count = total_kind1 - purge_count;

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
        cleanup_temp(&pool).await;
        return Ok(());
    }

    if !execute {
        println!("\n** DRY RUN — pass --execute to actually delete **");
        cleanup_temp(&pool).await;
        return Ok(());
    }

    println!("\nDeleting in batches of {batch_size}...");
    println!("(CASCADE will auto-clean event_refs, follows, follow_lists)\n");

    let start = Instant::now();
    let mut total_deleted: i64 = 0;

    loop {
        let result = sqlx::query(
            "DELETE FROM events
             WHERE id IN (
                 SELECT e.id FROM events e
                 INNER JOIN _purge_pubkeys p ON p.pubkey = e.pubkey
                 WHERE e.kind = 1
                 LIMIT $1
             )",
        )
        .bind(batch_size)
        .execute(&pool)
        .await?;

        let deleted = result.rows_affected() as i64;
        if deleted == 0 {
            break;
        }

        total_deleted += deleted;
        let elapsed = start.elapsed().as_secs();
        let rate = if elapsed > 0 {
            total_deleted / elapsed as i64
        } else {
            total_deleted
        };
        println!(
            "  deleted {total_deleted}/{purge_count} ({:.1}%) — {rate} events/sec",
            100.0 * total_deleted as f64 / purge_count as f64
        );
    }

    let elapsed = start.elapsed();
    println!(
        "\nDone! Deleted {total_deleted} kind-1 events in {:.1}s",
        elapsed.as_secs_f64()
    );

    // Clean up orphaned kind-0 metadata from purged authors
    println!("\nCleaning up orphaned kind-0 metadata...");
    let meta_result = sqlx::query(
        "DELETE FROM events
         WHERE kind = 0
           AND pubkey IN (SELECT pubkey FROM _purge_pubkeys)
           AND pubkey NOT IN (SELECT DISTINCT pubkey FROM events WHERE kind != 0)",
    )
    .execute(&pool)
    .await?;
    println!(
        "Cleaned up {} orphaned kind-0 metadata events",
        meta_result.rows_affected()
    );

    cleanup_temp(&pool).await;
    println!("\nRecommendation: run VACUUM FULL events; to reclaim disk space");

    Ok(())
}

async fn cleanup_temp(pool: &sqlx::PgPool) {
    let _ = sqlx::query("DROP TABLE IF EXISTS _purge_pubkeys")
        .execute(pool)
        .await;
}
