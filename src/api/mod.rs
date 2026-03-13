pub mod handlers;

use axum::middleware;
use axum::routing::{get, post};
use axum::Router;
use std::time::Duration;
use tower_http::cors::CorsLayer;

use crate::cache::StatsCache;
use crate::crawler::queue::CrawlQueue;
use crate::db::repository::EventRepository;
use crate::ratelimit::{rate_limit_middleware, RateLimiter};

/// Shared state available to all handlers.
#[derive(Clone)]
pub struct AppState {
    pub repo: EventRepository,
    pub cache: StatsCache,
    pub crawl_queue: Option<CrawlQueue>,
}

/// Build the axum router with all routes.
pub fn router(state: AppState) -> Router {
    // 30 requests per minute per IP
    let limiter = RateLimiter::new(120, Duration::from_secs(60));

    // Rate-limited API routes
    let api_routes = Router::new()
        .route("/v1/stats", get(handlers::get_stats))
        .route("/v1/events", get(handlers::get_events))
        .route("/v1/events/{id}", get(handlers::get_event_by_id))
        .route("/v1/events/{id}/thread", get(handlers::get_event_thread))
        .route("/v1/pages/note/{id}", get(handlers::get_note_detail))
        .route(
            "/v1/events/{id}/interactions",
            get(handlers::get_event_interactions),
        )
        .route(
            "/v1/events/{id}/refs/{ref_type}",
            get(handlers::get_event_refs),
        )
        .route("/v1/social/{pubkey}", get(handlers::get_social_graph))
        .route(
            "/v1/profiles/metadata",
            post(handlers::get_profiles_metadata),
        )
        .route("/v1/notes/top", get(handlers::get_top_notes_unified))
        .route("/v1/notes/trending", get(handlers::get_trending_notes))
        .route("/v1/users/new", get(handlers::get_new_users))
        .route("/v1/users/trending", get(handlers::get_trending_users))
        .route("/v1/users/zappers", get(handlers::get_top_zappers))
        .route("/v1/stats/daily", get(handlers::get_daily_stats))

        .route("/v1/notes/search", get(handlers::advanced_note_search))
        .route("/v1/search", get(handlers::search))
        .route("/v1/search/suggest", get(handlers::search_suggest))
        .route("/v1/crawler/stats", get(handlers::get_crawler_stats))
        .route_layer(middleware::from_fn_with_state(
            limiter,
            rate_limit_middleware,
        ));

    // Health check is NOT rate-limited (monitoring/uptime checks)
    Router::new()
        .route("/health", get(handlers::health))
        .merge(api_routes)
        .layer(CorsLayer::permissive())
        .with_state(state)
}
