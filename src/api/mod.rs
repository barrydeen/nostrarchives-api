pub mod handlers;

use axum::routing::{get, post};
use axum::Router;
use tower_http::cors::CorsLayer;

use crate::cache::StatsCache;
use crate::db::repository::EventRepository;

/// Shared state available to all handlers.
#[derive(Clone)]
pub struct AppState {
    pub repo: EventRepository,
    pub cache: StatsCache,
}

/// Build the axum router with all routes.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(handlers::health))
        .route("/v1/stats", get(handlers::get_stats))
        .route("/v1/events", get(handlers::get_events))
        .route("/v1/events/{id}", get(handlers::get_event_by_id))
        .route("/v1/events/{id}/thread", get(handlers::get_event_thread))
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
        .route("/v1/notes/trending", get(handlers::get_trending_notes))
        .route("/v1/users/new", get(handlers::get_new_users))
        .route("/v1/users/trending", get(handlers::get_trending_users))
        .route("/v1/stats/daily", get(handlers::get_daily_stats))
        .route("/v1/notes/likes/top", get(handlers::get_top_likes))
        .route(
            "/v1/notes/likes/top/today",
            get(handlers::get_top_likes_today),
        )
        .route("/v1/notes/zaps/top", get(handlers::get_top_zaps))
        .route(
            "/v1/notes/zaps/top/today",
            get(handlers::get_top_zaps_today),
        )
        .layer(CorsLayer::permissive())
        .with_state(state)
}
