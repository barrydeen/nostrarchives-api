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
        .layer(CorsLayer::permissive())
        .with_state(state)
}
