//! CLI tool to purge kind-1 notes from authors who don't pass the WoT filter.
//!
//! Strategy: drop FK CASCADE constraints, bulk delete, clean orphans, re-add FKs.
//! This avoids the CASCADE overhead that makes per-row deletes extremely slow.
//!
//! Usage:
//!   cargo run --bin purge_non_wot                  # dry run (default)
//!   cargo run --bin purge_non_wot -- --execute      # actually delete

use std::time::Instant;

use sqlx::postgres::PgPoolOptions;

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
    println!("Identifying non-WoT authors with kind-1 notes...");
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

    // === EXECUTE MODE ===
    let total_start = Instant::now();

    // Step 4: Drop FK constraints (eliminates CASCADE overhead)
    println!("\nDropping FK constraints...");
    let fks = [
        ("event_tags", "event_tags_event_id_fkey", "event_id", "id"),
        ("event_refs", "event_refs_source_event_id_fkey", "source_event_id", "id"),
        ("follow_lists", "follow_lists_event_id_fkey", "event_id", "id"),
        ("follows", "follows_source_event_id_fkey", "source_event_id", "id"),
    ];

    for (table, constraint, _, _) in &fks {
        let sql = format!("ALTER TABLE {table} DROP CONSTRAINT IF EXISTS {constraint}");
        sqlx::query(&sql).execute(&pool).await?;
        println!("  dropped {table}.{constraint}");
    }

    // Step 5: Bulk delete kind-1 events (no CASCADE = fast)
    println!("\nDeleting {purge_count} kind-1 notes...");
    let del_start = Instant::now();
    let result = sqlx::query(
        "DELETE FROM events e
         USING _purge_pubkeys p
         WHERE e.pubkey = p.pubkey AND e.kind = 1",
    )
    .execute(&pool)
    .await?;
    println!(
        "  deleted {} events in {:.1}s",
        result.rows_affected(),
        del_start.elapsed().as_secs_f64()
    );

    // Step 6: Clean up orphaned rows in referencing tables
    println!("\nCleaning orphaned event_refs...");
    let t = Instant::now();
    let r = sqlx::query(
        "DELETE FROM event_refs
         WHERE source_event_id NOT IN (SELECT id FROM events)",
    )
    .execute(&pool)
    .await?;
    println!("  removed {} event_refs in {:.1}s", r.rows_affected(), t.elapsed().as_secs_f64());

    println!("Cleaning orphaned follows...");
    let t = Instant::now();
    let r = sqlx::query(
        "DELETE FROM follows
         WHERE source_event_id NOT IN (SELECT id FROM events)",
    )
    .execute(&pool)
    .await?;
    println!("  removed {} follows in {:.1}s", r.rows_affected(), t.elapsed().as_secs_f64());

    println!("Cleaning orphaned follow_lists...");
    let t = Instant::now();
    let r = sqlx::query(
        "DELETE FROM follow_lists
         WHERE event_id NOT IN (SELECT id FROM events)",
    )
    .execute(&pool)
    .await?;
    println!("  removed {} follow_lists in {:.1}s", r.rows_affected(), t.elapsed().as_secs_f64());

    // Step 7: Clean up orphaned kind-0 metadata
    println!("Cleaning orphaned kind-0 metadata...");
    let t = Instant::now();
    let r = sqlx::query(
        "DELETE FROM events
         WHERE kind = 0
           AND pubkey IN (SELECT pubkey FROM _purge_pubkeys)
           AND pubkey NOT IN (SELECT DISTINCT pubkey FROM events WHERE kind != 0)",
    )
    .execute(&pool)
    .await?;
    println!("  removed {} metadata events in {:.1}s", r.rows_affected(), t.elapsed().as_secs_f64());

    // Step 8: Re-add FK constraints
    println!("\nRe-adding FK constraints...");
    for (table, constraint, col, ref_col) in &fks {
        let sql = format!(
            "ALTER TABLE {table} ADD CONSTRAINT {constraint}
             FOREIGN KEY ({col}) REFERENCES events ({ref_col}) ON DELETE CASCADE"
        );
        sqlx::query(&sql).execute(&pool).await?;
        println!("  restored {table}.{constraint}");
    }

    cleanup_temp(&pool).await;

    println!(
        "\nDone! Total time: {:.1}s",
        total_start.elapsed().as_secs_f64()
    );
    println!("Recommendation: run VACUUM FULL events; to reclaim disk space");

    Ok(())
}

async fn cleanup_temp(pool: &sqlx::PgPool) {
    let _ = sqlx::query("DROP TABLE IF EXISTS _purge_pubkeys")
        .execute(pool)
        .await;
}
