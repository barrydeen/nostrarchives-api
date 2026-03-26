pub mod handlers;

use axum::middleware;
use axum::routing::{get, post};
use axum::Router;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tower_http::cors::CorsLayer;

use crate::auth::ReplayGuard;
use crate::block_cache::BlockCache;
use crate::cache::StatsCache;
use crate::crawler::queue::CrawlQueue;
use crate::db::repository::EventRepository;
use crate::live_metrics::LiveMetricsTracker;
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
    pub live_tracker: Option<Arc<LiveMetricsTracker>>,
    pub block_cache: BlockCache,
    pub admin_pubkey: Option<String>,
    pub replay_guard: ReplayGuard,
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

/// WebSocket upgrade handler for live metrics streaming.
async fn ws_live_metrics(
    ws: axum::extract::WebSocketUpgrade,
    axum::extract::State(state): axum::extract::State<AppState>,
) -> impl axum::response::IntoResponse {
    ws.on_upgrade(move |socket| async move {
        if let Some(tracker) = state.live_tracker {
            crate::live_metrics::handle_live_metrics_ws(socket, tracker).await;
        }
    })
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

    // WebSocket routes are NOT rate-limited
    let ws_routes = Router::new()
        .route("/v1/ws/live-metrics", get(ws_live_metrics));

    // Admin routes — auth enforced per-handler via AdminAuth extractor
    // Stricter rate limit: 10 req/min per IP to prevent brute-force sig verification DoS
    let admin_limiter = RateLimiter::new(10, Duration::from_secs(60));
    let admin_routes = Router::new()
        .route("/v1/admin/check-auth", get(handlers::admin_check_auth))
        .route(
            "/v1/admin/block-pubkey",
            post(handlers::admin_block_pubkey).delete(handlers::admin_unblock_pubkey),
        )
        .route("/v1/admin/blocked-pubkeys", get(handlers::admin_list_blocked_pubkeys))
        .route("/v1/admin/purge-status/{pubkey}", get(handlers::admin_purge_status))
        .route(
            "/v1/admin/block-hashtag",
            post(handlers::admin_block_hashtag).delete(handlers::admin_unblock_hashtag),
        )
        .route("/v1/admin/blocked-hashtags", get(handlers::admin_list_blocked_hashtags))
        .route(
            "/v1/admin/block-search-term",
            post(handlers::admin_block_search_term).delete(handlers::admin_unblock_search_term),
        )
        .route("/v1/admin/blocked-search-terms", get(handlers::admin_list_blocked_search_terms))
        .route_layer(middleware::from_fn_with_state(
            admin_limiter,
            rate_limit_middleware,
        ));

    // Health check is NOT rate-limited (monitoring/uptime checks)
    Router::new()
        .route("/health", get(handlers::health))
        .merge(ws_routes)
        .merge(api_routes)
        .merge(admin_routes)
        .layer(CorsLayer::permissive())
        .with_state(state)
}
