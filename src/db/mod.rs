pub mod models;
pub mod repository;

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::time::Duration;

/// Initialize the database connection pool and run migrations.
pub async fn init_pool(database_url: &str) -> Result<PgPool, sqlx::Error> {
    let pool = PgPoolOptions::new()
        .max_connections(30)
        .acquire_timeout(Duration::from_secs(10))
        .connect(database_url)
        .await?;

    run_migrations(&pool).await?;

    Ok(pool)
}

/// Run SQL migration files in order.
async fn run_migrations(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS _migrations (
            name TEXT PRIMARY KEY,
            applied_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await?;

    let migrations = vec![
        (
            "001_create_events",
            include_str!("../../migrations/001_create_events.sql"),
        ),
        (
            "002_create_event_tags",
            include_str!("../../migrations/002_create_event_tags.sql"),
        ),
        (
            "003_create_event_refs",
            include_str!("../../migrations/003_create_event_refs.sql"),
        ),
        (
            "004_create_follows",
            include_str!("../../migrations/004_create_follows.sql"),
        ),
        (
            "005_backfill_zap_amounts",
            include_str!("../../migrations/005_backfill_zap_amounts.sql"),
        ),
        (
            "006_profile_search",
            include_str!("../../migrations/006_profile_search.sql"),
        ),
        (
            "007_crawl_state",
            include_str!("../../migrations/007_crawl_state.sql"),
        ),
        (
            "008_char64_to_text",
            include_str!("../../migrations/008_char64_to_text.sql"),
        ),
        (
            "009_trending_indexes",
            include_str!("../../migrations/009_trending_indexes.sql"),
        ),
        (
            "010_relay_capabilities",
            include_str!("../../migrations/010_relay_capabilities.sql"),
        ),
        (
            "011_negentropy_sync_state",
            include_str!("../../migrations/011_negentropy_sync_state.sql"),
        ),
        (
            "012_daily_analytics",
            include_str!("../../migrations/012_daily_analytics.sql"),
        ),
        (
            "013_profile_tab_optimizations",
            include_str!("../../migrations/013_profile_tab_optimizations.sql"),
        ),
        (
            "014_v2_counter_columns",
            include_str!("../../migrations/014_v2_counter_columns.sql"),
        ),
        (
            "015_seen_events",
            include_str!("../../migrations/015_seen_events.sql"),
        ),
        (
            "016_wot_scores",
            include_str!("../../migrations/016_wot_scores.sql"),
        ),
        (
            "017_update_profile_search",
            include_str!("../../migrations/017_update_profile_search.sql"),
        ),
        (
            "019_analytics_materialized_views",
            include_str!("../../migrations/019_analytics_materialized_views.sql"),
        ),
        (
            "020_scheduled_events",
            include_str!("../../migrations/020_scheduled_events.sql"),
        ),
        (
            "021_exponential_backfill",
            include_str!("../../migrations/021_exponential_backfill.sql"),
        ),
        (
            "022_missing_events",
            include_str!("../../migrations/022_missing_events.sql"),
        ),
        (
            "023_reset_zap_negentropy",
            include_str!("../../migrations/023_reset_zap_negentropy.sql"),
        ),
        (
            "024_analytics_leaderboard_views",
            include_str!("../../migrations/024_analytics_leaderboard_views.sql"),
        ),
        (
            "025_profile_tab_sort_indexes",
            include_str!("../../migrations/025_profile_tab_sort_indexes.sql"),
        ),
        (
            "026_zap_metadata_created_at_index",
            include_str!("../../migrations/026_zap_metadata_created_at_index.sql"),
        ),
        (
            "027_hashtag_gin_index",
            include_str!("../../migrations/027_hashtag_gin_index.sql"),
        ),
        (
            "028_perf_missing_indexes",
            include_str!("../../migrations/028_perf_missing_indexes.sql"),
        ),
        (
            "029_follows_composite_indexes",
            include_str!("../../migrations/029_follows_composite_indexes.sql"),
        ),
        (
            "030_trending_covering_index",
            include_str!("../../migrations/030_trending_covering_index.sql"),
        ),
        (
            "031_trending_composite_indexes",
            include_str!("../../migrations/031_trending_composite_indexes.sql"),
        ),
        (
            "032_drop_event_tags",
            include_str!("../../migrations/032_drop_event_tags.sql"),
        ),
        (
            "033_search_index",
            include_str!("../../migrations/033_search_index.sql"),
        ),
        (
            "034_note_hashtags",
            include_str!("../../migrations/034_note_hashtags.sql"),
        ),
        (
            "035_simplify_profile_search",
            include_str!("../../migrations/035_simplify_profile_search.sql"),
        ),
        (
            "036_follows_source_event_id_index",
            include_str!("../../migrations/036_follows_source_event_id_index.sql"),
        ),
        (
            "037_blocked_pubkeys",
            include_str!("../../migrations/037_blocked_pubkeys.sql"),
        ),
        (
            "038_blocked_hashtags",
            include_str!("../../migrations/038_blocked_hashtags.sql"),
        ),
        (
            "039_blocked_search_terms",
            include_str!("../../migrations/039_blocked_search_terms.sql"),
        ),
        (
            "040_crawl_state_zaps_crawled_at",
            include_str!("../../migrations/040_crawl_state_zaps_crawled_at.sql"),
        ),
        (
            "041_client_top_users_view",
            include_str!("../../migrations/041_client_top_users_view.sql"),
        ),
    ];

    for (name, sql) in migrations {
        let applied: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM _migrations WHERE name = $1)")
                .bind(name)
                .fetch_one(pool)
                .await?;

        if !applied {
            tracing::info!("applying migration: {name}");

            if sql.starts_with("-- no-transaction") {
                // CONCURRENTLY operations cannot run inside a transaction block.
                // Execute each statement individually on a dedicated connection.
                let mut conn = pool.acquire().await?;
                for stmt in sql.split(';') {
                    let stmt = stmt.trim();
                    if stmt.is_empty() || stmt.starts_with("--") {
                        continue;
                    }
                    sqlx::raw_sql(stmt).execute(&mut *conn).await?;
                }
            } else {
                sqlx::raw_sql(sql).execute(pool).await?;
            }

            sqlx::query("INSERT INTO _migrations (name) VALUES ($1)")
                .bind(name)
                .execute(pool)
                .await?;
        }
    }

    Ok(())
}
