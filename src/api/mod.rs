pub mod handlers;

use axum::middleware;
use axum::routing::{get, post};
use axum::Router;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tower_http::cors::CorsLayer;

use crate::cache::StatsCache;
use crate::crawler::queue::CrawlQueue;
use crate::db::repository::EventRepository;
use crate::profile_search_cache::ProfileSearchCache;
use crate::ratelimit::{rate_limit_middleware, RateLimiter};
use crate::relay::fetcher::RelayFetcher;

/// Shared state available to all handlers.
#[derive(Clone)]
pub struct AppState {
    pub repo: EventRepository,
    pub cache: StatsCache,
    pub crawl_queue: Option<CrawlQueue>,
    pub fetcher: Arc<RelayFetcher>,
    pub profile_search_cache: ProfileSearchCache,
}

async fn cache_control_middleware(
    req: axum::extract::Request,
    next: middleware::Next,
) -> axum::response::Response {
    let path = req.uri().path().to_string();
    let mut response = next.run(req).await;

    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        "no-store".parse().unwrap(),
    );

    response
}

/// Build the axum router with all routes.
pub fn router(state: AppState) -> Router {
    // 120 requests per minute per IP
    // Whitelist trusted server IPs (frontend SSR, localhost) to bypass rate limiting
    let whitelist: Vec<IpAddr> = std::env::var("RATELIMIT_WHITELIST")
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    if !whitelist.is_empty() {
        tracing::info!("rate limiter: whitelisted IPs: {:?}", whitelist);
    }
    let limiter = RateLimiter::new(120, Duration::from_secs(60))
        .with_whitelist(whitelist);

    // Rate-limited API routes
    let api_routes = Router::new()
        .route("/v1/stats", get(handlers::get_stats))
        .route("/v1/stats/follower-cache", get(handlers::get_follower_cache_stats))
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
        .route("/v1/hashtags/trending", get(handlers::get_trending_hashtags))
        .route("/v1/hashtags/{tag}/notes", get(handlers::get_hashtag_notes))
        .route("/v1/stats/daily", get(handlers::get_daily_stats))

        .route("/v1/clients/leaderboard", get(handlers::get_client_leaderboard))
        .route("/v1/relays/leaderboard", get(handlers::get_relay_leaderboard))
        .route("/v1/analytics/daily", get(handlers::get_analytics_daily))
        .route("/v1/analytics/top-posters", get(handlers::get_top_posters))
        .route("/v1/analytics/most-liked", get(handlers::get_most_liked_authors))
        .route("/v1/analytics/most-shared", get(handlers::get_most_shared_authors))
        .route("/v1/notes/search", get(handlers::advanced_note_search))
        .route("/v1/search", get(handlers::search))
        .route("/v1/search/suggest", get(handlers::search_suggest))
        .route("/v1/profiles/{pubkey}/notes", get(handlers::get_profile_notes))
        .route("/v1/profiles/{pubkey}/replies", get(handlers::get_profile_replies))
        .route("/v1/profiles/{pubkey}/zaps/sent", get(handlers::get_profile_zaps_sent))
        .route("/v1/profiles/{pubkey}/zaps/received", get(handlers::get_profile_zaps_received))
        .route("/v1/profiles/{pubkey}/zap-stats", get(handlers::get_profile_zap_stats))
        .route("/v1/crawler/stats", get(handlers::get_crawler_stats))
        .route_layer(middleware::from_fn_with_state(
            limiter,
            rate_limit_middleware,
        ))
        .layer(middleware::from_fn(cache_control_middleware));

    // Health check is NOT rate-limited (monitoring/uptime checks)
    Router::new()
        .route("/health", get(handlers::health))
        .merge(api_routes)
        .layer(CorsLayer::permissive())
        .with_state(state)
}
