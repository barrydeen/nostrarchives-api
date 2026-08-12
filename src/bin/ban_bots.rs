//! Bulk-ban and purge authors confirmed as bots by the nspam classifier.
//!
//! Reads `author_spam_scores` where `decision = 'confirmed'` (set by
//! `scripts/nspam/review.py promote` after a human has looked at the
//! distribution), blocks each author, then deletes all their data through the
//! same purge path the admin endpoint uses.
//!
//! Ordering is load-bearing: each pubkey is inserted into `blocked_pubkeys`
//! BEFORE its data is deleted. Deleting without blocking is self-reversing —
//! negentropy rebuilds its local ID set from `events`, so deleted ids come back
//! as `need_ids` on the next sync.
//!
//! Usage:
//!   cargo run --release --bin ban_bots                                  # dry run
//!   cargo run --release --bin ban_bots -- --execute --max-bans 500
//!   cargo run --release --bin ban_bots -- --execute --max-bans 500 --sleep 200
//!
//! Flags:
//!   --execute            actually block and delete (default: dry run)
//!   --max-bans N         hard cap on authors banned this run (required with --execute)
//!   --sleep MS           pause between authors (default 100)
//!   --max-fraction F     refuse if the ban set exceeds this fraction of all
//!                        authors (default 0.05)
//!   --archive PATH       write deleted events to JSONL before deleting
//!   --skip-archive       proceed without an archive (you are on your own)
//!   --bridged            ban authors whose content is entirely bridged in from
//!                        another network (NIP-48 proxy tags) instead of using
//!                        classifier scores. Deterministic; no model involved.
//!   --scored-on MODE     which score mode to ban on (default: replies)
//!   --allow-out-of-distribution
//!                        required to ban on 'roots'/'all' — see the guard below

use std::io::Write;
use std::time::Instant;

use sqlx::postgres::PgPoolOptions;
use sqlx::Row;
use tokio::time::{sleep, Duration};

const DEFAULT_SLEEP_MS: u64 = 100;
const BATCH_SIZE: i64 = 5000;
const MAX_SCORE_AGE_DAYS: i64 = 7;

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

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
    let skip_archive = args.iter().any(|a| a == "--skip-archive");
    let sleep_ms = arg_value(&args, "--sleep")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_SLEEP_MS);
    let max_bans = arg_value(&args, "--max-bans").and_then(|v| v.parse::<i64>().ok());
    let max_fraction = arg_value(&args, "--max-fraction")
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(0.05);
    let archive_path = arg_value(&args, "--archive")
        .unwrap_or_else(|| "bot_purge_archive.jsonl".to_string());
    let bridged_mode = args.iter().any(|a| a == "--bridged");
    let scored_on = arg_value(&args, "--scored-on").unwrap_or_else(|| "replies".to_string());
    let allow_ood = args.iter().any(|a| a == "--allow-out-of-distribution");

    // The model is trained on replies. Measured on a real 955k-event corpus,
    // root-note scoring flagged 29% of authors at score >= 0.99 — including 53
    // accounts with over 1,000 followers — against 2.3% for replies. Those were
    // sampled and read: they were ordinary humans posting music, photos and
    // commentary. Banning on a non-reply mode requires an explicit opt-in.
    if scored_on != "replies" && !allow_ood {
        println!(
            "ERROR: --scored-on {scored_on} is out-of-distribution for this model.\n\
             Measured false-positive rate on root notes is roughly an order of\n\
             magnitude worse than on replies. Pass --allow-out-of-distribution\n\
             only after sampling the flagged set by hand. Refusing."
        );
        return Ok(());
    }

    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let pool = PgPoolOptions::new()
        .max_connections(3)
        .acquire_timeout(Duration::from_secs(30))
        .connect(&database_url)
        .await?;

    // ── Bridged mode is deterministic; the classifier guards do not apply. ──
    if bridged_mode {
        return run_bridged(&pool, execute, max_bans, max_fraction, sleep_ms,
                           &archive_path, skip_archive).await;
    }

    // ── Safety guards. All must pass before anything is deleted. ──
    let (scored,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM author_spam_scores WHERE scored_on = $1",
    )
    .bind(&scored_on)
    .fetch_one(&pool)
    .await?;
    if scored == 0 {
        println!("ERROR: author_spam_scores is empty — run the scorer first. Refusing.");
        return Ok(());
    }

    let stale: Option<(i64,)> = sqlx::query_as(
        "SELECT EXTRACT(DAY FROM NOW() - MAX(scored_at))::bigint
         FROM author_spam_scores WHERE scored_on = $1",
    )
    .bind(&scored_on)
    .fetch_optional(&pool)
    .await?;
    let age_days = stale.map(|(d,)| d).unwrap_or(0);
    if age_days > MAX_SCORE_AGE_DAYS {
        println!(
            "ERROR: newest score is {age_days} days old (limit {MAX_SCORE_AGE_DAYS}). \
             Re-score before banning. Refusing."
        );
        return Ok(());
    }

    let (confirmed,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM author_spam_scores
         WHERE decision = 'confirmed' AND scored_on = $1",
    )
    .bind(&scored_on)
    .fetch_one(&pool)
    .await?;
    let (total_authors,): (i64,) =
        sqlx::query_as("SELECT COUNT(DISTINCT pubkey) FROM events")
            .fetch_one(&pool)
            .await?;

    println!("scored_on mode:    {scored_on}");
    println!("scored authors:    {scored}");
    println!("confirmed bots:    {confirmed}");
    println!("total authors:     {total_authors}");
    println!("newest score age:  {age_days}d");

    if confirmed == 0 {
        println!("\nNothing marked 'confirmed'. Run review.py promote first.");
        return Ok(());
    }

    // A run that wants to ban a large slice of the user base is a bug, not a
    // spam wave.
    if total_authors > 0 {
        let frac = confirmed as f64 / total_authors as f64;
        if frac > max_fraction {
            println!(
                "\nERROR: ban set is {:.1}% of all authors, above the {:.0}% limit. Refusing.",
                frac * 100.0,
                max_fraction * 100.0
            );
            return Ok(());
        }
    }

    if execute && max_bans.is_none() {
        println!("\nERROR: --max-bans is required with --execute. Refusing.");
        return Ok(());
    }
    let limit = max_bans.unwrap_or(confirmed);

    // Guardrails are enforced here too, not only in review.py: this is the tool
    // that actually deletes, and the two can be run independently.
    let rows = sqlx::query(
        "SELECT s.pubkey, s.score, s.follower_count
         FROM author_spam_scores s
         WHERE s.decision = 'confirmed'
           AND s.scored_on = $1
           AND NOT EXISTS (SELECT 1 FROM spam_allowlist a WHERE a.pubkey = s.pubkey)
         ORDER BY s.score DESC
         LIMIT $2",
    )
    .bind(&scored_on)
    .bind(limit)
    .fetch_all(&pool)
    .await?;

    let targets: Vec<(String, f32, i32)> = rows
        .iter()
        .map(|r| {
            (
                r.get::<String, _>("pubkey"),
                r.get::<f32, _>("score"),
                r.get::<i32, _>("follower_count"),
            )
        })
        .collect();

    let (event_total,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM events WHERE pubkey = ANY($1)",
    )
    .bind(targets.iter().map(|t| t.0.clone()).collect::<Vec<_>>())
    .fetch_one(&pool)
    .await?;

    println!("\nwill ban {} authors, deleting {event_total} events", targets.len());
    println!("score range: {:.4} … {:.4}", targets.last().map(|t| t.1).unwrap_or(0.0), targets.first().map(|t| t.1).unwrap_or(0.0));

    if !execute {
        println!("\nDRY RUN — nothing changed. Sample of what would be banned:\n");
        for (pk, score, flwr) in targets.iter().take(15) {
            println!("  {pk}  score {score:.4}  followers {flwr}");
        }
        println!("\nRe-run with --execute --max-bans N to proceed.");
        return Ok(());
    }

    // ── Archive before deleting. This is the only undo. ──
    if !skip_archive {
        println!("\narchiving events to {archive_path} …");
        let t0 = Instant::now();
        let file = std::fs::File::create(&archive_path)?;
        let mut w = std::io::BufWriter::new(file);
        let mut archived = 0i64;

        // Rebuild each event from its columns rather than trusting `raw`. The
        // archive is the only undo, so it must not depend on `raw` having been
        // populated completely at ingestion time.
        let mut empty_raw = 0i64;
        for (pk, _, _) in &targets {
            let evs = sqlx::query(
                "SELECT id, pubkey, created_at, kind, content, sig, tags, raw,
                        is_reply, is_machine_note, reaction_count, repost_count,
                        reply_count, zap_count, zap_amount_msats
                 FROM events WHERE pubkey = $1",
            )
            .bind(pk)
            .fetch_all(&pool)
            .await?;
            for e in evs {
                let raw: serde_json::Value = e.get("raw");
                if raw.as_object().map(|o| o.is_empty()).unwrap_or(true) {
                    empty_raw += 1;
                }
                let rebuilt = serde_json::json!({
                    "id": e.get::<String, _>("id"),
                    "pubkey": e.get::<String, _>("pubkey"),
                    "created_at": e.get::<i64, _>("created_at"),
                    "kind": e.get::<i32, _>("kind"),
                    "content": e.get::<String, _>("content"),
                    "sig": e.get::<String, _>("sig"),
                    "tags": e.get::<serde_json::Value, _>("tags"),
                    "raw": raw,
                    // Restoring without these loses reply threading and all
                    // engagement history — search_index rebuilds from triggers,
                    // but is_reply and the counters have no other source.
                    "is_reply": e.get::<bool, _>("is_reply"),
                    "is_machine_note": e.get::<bool, _>("is_machine_note"),
                    "reaction_count": e.get::<i32, _>("reaction_count"),
                    "repost_count": e.get::<i32, _>("repost_count"),
                    "reply_count": e.get::<i32, _>("reply_count"),
                    "zap_count": e.get::<i32, _>("zap_count"),
                    "zap_amount_msats": e.get::<i64, _>("zap_amount_msats"),
                });
                writeln!(w, "{rebuilt}")?;
                archived += 1;
            }
        }
        w.flush()?;
        println!("archived {archived} events in {:.1}s", t0.elapsed().as_secs_f64());
        if empty_raw > 0 {
            println!(
                "note: {empty_raw} events had an empty `raw` column; \
                 they were reconstructed from their stored columns."
            );
        }

        if archived < event_total {
            println!(
                "ERROR: archived {archived} but expected {event_total}. Refusing to delete."
            );
            return Ok(());
        }
    } else {
        println!("\n--skip-archive: proceeding with NO backup.");
    }

    // ── Block, then purge, one author at a time. ──
    println!("\nbanning …");
    let t0 = Instant::now();
    let mut banned = 0i64;
    let mut deleted_total = 0i64;

    for (i, (pk, score, _)) in targets.iter().enumerate() {
        // Block FIRST — see the module docs. Without this the purge undoes itself.
        sqlx::query(
            "INSERT INTO blocked_pubkeys (pubkey, reason, blocked_by)
             VALUES ($1, $2, 'ban_bots')
             ON CONFLICT (pubkey) DO UPDATE SET reason = EXCLUDED.reason, blocked_at = NOW()",
        )
        .bind(pk)
        .bind(format!("nspam score {score:.4}"))
        .execute(&pool)
        .await?;

        let mut last = 0i64;
        match nostr_api::block_cache::purge_pubkey_data_with(
            &pool,
            pk,
            BATCH_SIZE,
            &mut |n| last = n,
        )
        .await
        {
            Ok(n) => {
                deleted_total += n;
                banned += 1;
                sqlx::query(
                    "UPDATE author_spam_scores SET decision = 'purged', decided_at = NOW()
                     WHERE pubkey = $1 AND scored_on = $2",
                )
                .bind(pk)
                .bind(&scored_on)
                .execute(&pool)
                .await?;
            }
            Err(e) => {
                println!("  FAILED {pk}: {e}");
                continue;
            }
        }

        if (i + 1) % 25 == 0 || i + 1 == targets.len() {
            println!(
                "  {}/{} authors, {deleted_total} events, {:.0}s elapsed",
                i + 1,
                targets.len(),
                t0.elapsed().as_secs_f64()
            );
        }

        if sleep_ms > 0 {
            sleep(Duration::from_millis(sleep_ms)).await;
        }
    }

    println!(
        "\nbanned {banned} authors, deleted {deleted_total} events in {:.1}s",
        t0.elapsed().as_secs_f64()
    );
    println!(
        "\nNext steps (in this order):\n\
         1. Flush Redis caches and DEL nostr:unique_pubkeys (HyperLogLog cannot \
            have elements removed).\n\
         2. REFRESH MATERIALIZED VIEW CONCURRENTLY profile_search;  -- must be first\n\
         3. Refresh the analytics views (three of them join profile_search).\n\
         4. Restart the API so ProfileSearchCache / WotCache / FollowerCache reload.\n\
         5. VACUUM (ANALYZE) events, event_refs, note_hashtags, search_index, \
            zap_metadata, seen_events, follows, follow_lists;"
    );

    Ok(())
}

/// Ban authors whose content is entirely bridged in from another network.
///
/// Selection is by NIP-48 `proxy` tags, which the bridge itself attaches, so
/// membership is a fact about the event rather than a prediction. There is no
/// classifier and no threshold here. Authors who post any native content are
/// excluded, so a person who occasionally cross-posts keeps their account.
async fn run_bridged(
    pool: &sqlx::PgPool,
    execute: bool,
    max_bans: Option<i64>,
    max_fraction: f64,
    sleep_ms: u64,
    archive_path: &str,
    skip_archive: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // An account counts as bridged when it carries proxy-tagged events and has
    // written no native kind-1 notes. The native test is deliberately scoped to
    // kind 1: bridges also emit kind-0 profiles and kind-10002 relay lists
    // without proxy tags, so testing across all kinds would spare almost every
    // mirrored account on a technicality. Measured on a 643k-event corpus,
    // exactly one kind-1 author mixes bridged and native notes.
    const SELECT_BRIDGED: &str = "
        WITH per_author AS (
            SELECT e.pubkey,
                   COUNT(*) AS total_events,
                   COUNT(*) FILTER (WHERE EXISTS (
                       SELECT 1 FROM jsonb_array_elements(e.tags) t WHERE t->>0 = 'proxy')) AS proxied,
                   COUNT(*) FILTER (WHERE e.kind = 1 AND NOT EXISTS (
                       SELECT 1 FROM jsonb_array_elements(e.tags) t WHERE t->>0 = 'proxy')) AS native_notes
            FROM events e
            GROUP BY e.pubkey
        )
        SELECT pubkey, total_events AS proxied
        FROM per_author
        WHERE proxied > 0 AND native_notes = 0
          AND NOT EXISTS (SELECT 1 FROM spam_allowlist a WHERE a.pubkey = per_author.pubkey)
          AND NOT EXISTS (SELECT 1 FROM blocked_pubkeys b WHERE b.pubkey = per_author.pubkey)
        ORDER BY total_events DESC
        LIMIT $1
    ";

    let (total_authors,): (i64,) =
        sqlx::query_as("SELECT COUNT(DISTINCT pubkey) FROM events")
            .fetch_one(pool)
            .await?;

    let limit = max_bans.unwrap_or(i64::MAX);
    let rows = sqlx::query(SELECT_BRIDGED).bind(limit).fetch_all(pool).await?;
    let targets: Vec<(String, i64)> = rows
        .iter()
        .map(|r| (r.get::<String, _>("pubkey"), r.get::<i64, _>("proxied")))
        .collect();

    let events_total: i64 = targets.iter().map(|t| t.1).sum();
    println!("mode:              bridged (NIP-48 proxy tags)");
    println!("purely bridged:    {} authors", targets.len());
    println!("their events:      {events_total}");
    println!("total authors:     {total_authors}");

    if targets.is_empty() {
        println!("\nNothing to do.");
        return Ok(());
    }
    if total_authors > 0 {
        let frac = targets.len() as f64 / total_authors as f64;
        if frac > max_fraction {
            println!(
                "\nERROR: ban set is {:.1}% of all authors, above the {:.0}% limit.\n\
                 Bridged content is a large share of the corpus, so raise\n\
                 --max-fraction deliberately if this is intended. Refusing.",
                frac * 100.0,
                max_fraction * 100.0
            );
            return Ok(());
        }
    }
    if !execute {
        println!("\nDRY RUN — nothing changed. Sample of what would be banned:\n");
        for (pk, n) in targets.iter().take(15) {
            let name: Option<(String,)> = sqlx::query_as(
                "SELECT COALESCE(content::jsonb->>'name', '') FROM events
                 WHERE pubkey = $1 AND kind = 0 ORDER BY created_at DESC LIMIT 1",
            )
            .bind(pk)
            .fetch_optional(pool)
            .await
            .unwrap_or(None);
            let label = name
                .map(|(n,)| n)
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| "(no name)".into());
            println!("  {:<34} {:>6} events  {pk}", label.chars().take(32).collect::<String>(), n);
        }
        if targets.len() > 15 {
            println!("  … and {} more", targets.len() - 15);
        }
        println!("\nRe-run with --execute --max-bans N to proceed.");
        return Ok(());
    }
    if max_bans.is_none() {
        println!("\nERROR: --max-bans is required with --execute. Refusing.");
        return Ok(());
    }

    if !skip_archive {
        println!("\narchiving to {archive_path} …");
        let file = std::fs::File::create(archive_path)?;
        let mut w = std::io::BufWriter::new(file);
        let mut archived = 0i64;
        for (pk, _) in &targets {
            for e in sqlx::query(
                "SELECT id, pubkey, created_at, kind, content, sig, tags, raw, is_reply,
                        is_machine_note, reaction_count, repost_count, reply_count,
                        zap_count, zap_amount_msats
                 FROM events WHERE pubkey = $1",
            )
            .bind(pk)
            .fetch_all(pool)
            .await?
            {
                let rebuilt = serde_json::json!({
                    "id": e.get::<String, _>("id"),
                    "pubkey": e.get::<String, _>("pubkey"),
                    "created_at": e.get::<i64, _>("created_at"),
                    "kind": e.get::<i32, _>("kind"),
                    "content": e.get::<String, _>("content"),
                    "sig": e.get::<String, _>("sig"),
                    "tags": e.get::<serde_json::Value, _>("tags"),
                    "raw": e.get::<serde_json::Value, _>("raw"),
                    "is_reply": e.get::<bool, _>("is_reply"),
                    "is_machine_note": e.get::<bool, _>("is_machine_note"),
                    "reaction_count": e.get::<i32, _>("reaction_count"),
                    "repost_count": e.get::<i32, _>("repost_count"),
                    "reply_count": e.get::<i32, _>("reply_count"),
                    "zap_count": e.get::<i32, _>("zap_count"),
                    "zap_amount_msats": e.get::<i64, _>("zap_amount_msats"),
                });
                writeln!(w, "{rebuilt}")?;
                archived += 1;
            }
        }
        w.flush()?;
        println!("archived {archived} events");
    }

    let t0 = Instant::now();
    let mut deleted_total = 0i64;
    for (i, (pk, _)) in targets.iter().enumerate() {
        // Block first — see the module docs; deleting without blocking is
        // self-reversing.
        sqlx::query(
            "INSERT INTO blocked_pubkeys (pubkey, reason, blocked_by)
             VALUES ($1, 'bridged content (NIP-48 proxy)', 'ban_bots')
             ON CONFLICT (pubkey) DO UPDATE SET blocked_at = NOW()",
        )
        .bind(pk)
        .execute(pool)
        .await?;

        match nostr_api::block_cache::purge_pubkey_data_with(pool, pk, BATCH_SIZE, &mut |_| {})
            .await
        {
            Ok(n) => deleted_total += n,
            Err(e) => println!("  FAILED {pk}: {e}"),
        }
        if (i + 1) % 100 == 0 || i + 1 == targets.len() {
            println!(
                "  {}/{} authors, {deleted_total} events, {:.0}s",
                i + 1,
                targets.len(),
                t0.elapsed().as_secs_f64()
            );
        }
        if sleep_ms > 0 {
            sleep(Duration::from_millis(sleep_ms)).await;
        }
    }
    println!(
        "\nbanned {} authors, deleted {deleted_total} events in {:.1}s",
        targets.len(),
        t0.elapsed().as_secs_f64()
    );
    Ok(())
}
