pub mod handlers;

use axum::routing::get;
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
        .route("/api/v1/stats", get(handlers::get_stats))
        .route("/api/v1/events", get(handlers::get_events))
        .route("/api/v1/events/{id}", get(handlers::get_event_by_id))
        .route(
            "/api/v1/events/{id}/thread",
            get(handlers::get_event_thread),
        )
        .route(
            "/api/v1/events/{id}/interactions",
            get(handlers::get_event_interactions),
        )
        .route(
            "/api/v1/events/{id}/refs/{ref_type}",
            get(handlers::get_event_refs),
        )
        .route("/api/v1/social/{pubkey}", get(handlers::get_social_graph))
        .layer(CorsLayer::permissive())
        .with_state(state)
}
