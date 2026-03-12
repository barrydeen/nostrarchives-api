pub mod models;
pub mod repository;

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

/// Initialize the database connection pool and run migrations.
pub async fn init_pool(database_url: &str) -> Result<PgPool, sqlx::Error> {
    let pool = PgPoolOptions::new()
        .max_connections(20)
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
    ];

    for (name, sql) in migrations {
        let applied: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM _migrations WHERE name = $1)")
                .bind(name)
                .fetch_one(pool)
                .await?;

        if !applied {
            tracing::info!("applying migration: {name}");
            sqlx::raw_sql(sql).execute(pool).await?;
            sqlx::query("INSERT INTO _migrations (name) VALUES ($1)")
                .bind(name)
                .execute(pool)
                .await?;
        }
    }

    Ok(())
}
