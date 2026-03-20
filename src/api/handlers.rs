use std::collections::{HashMap, HashSet};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use bech32::{self, FromBase32};
use chrono::Utc;
use hex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Sha256, Digest};
use tracing::warn;

use super::AppState;
use crate::db::models::{EventQuery, NoteSearchResult, ProfileSearchResult};
use crate::error::AppError;
use crate::nip19;

/// Health check endpoint.
pub async fn health() -> (StatusCode, Json<Value>) {
    (StatusCode::OK, Json(json!({ "status": "ok" })))
}

/// Get cached global statistics.
pub async fn get_stats(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let stats = state.cache.get_stats().await?;
    Ok(Json(serde_json::to_value(stats).unwrap()))
}

/// Get follower cache statistics for monitoring.
pub async fn get_follower_cache_stats(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let wot_stats = state.repo.wot_cache.stats().await;
    let follower_stats = state.repo.follower_cache.stats().await;
    let profile_search_stats = state.profile_search_cache.stats().await;
    Ok(Json(json!({
        "wot": {
            "passing_count": wot_stats.passing_count,
            "threshold": wot_stats.threshold,
            "last_refresh_ago_secs": wot_stats.last_refresh_ago.as_secs(),
            "refresh_interval_secs": wot_stats.refresh_interval.as_secs(),
        },
        "follower_cache": {
            "qualified_count": follower_stats.qualified_count,
            "threshold": follower_stats.threshold,
            "last_refresh_ago_secs": follower_stats.last_refresh_ago.as_secs(),
            "refresh_interval_secs": follower_stats.refresh_interval.as_secs(),
        },
        "profile_search": {
            "profile_count": profile_search_stats.profile_count,
            "last_refresh_ago_secs": profile_search_stats.last_refresh_ago.as_secs(),
            "refresh_interval_secs": profile_search_stats.refresh_interval.as_secs(),
        }
    })))
}

/// Query events with filters.
pub async fn get_events(
    State(state): State<AppState>,
    Query(q): Query<EventQuery>,
) -> Result<Json<Value>, AppError> {
    let (events, total) = tokio::try_join!(
        state.repo.query_events(&q),
        state
            .repo
            .count_events_filtered(q.pubkey.as_deref(), q.kind, q.since, q.until,),
    )?;

    // Batch-fetch engagement stats for all returned events
    let event_ids: Vec<String> = events.iter().map(|e| e.id.clone()).collect();
    let interactions = state.repo.batch_get_interactions(&event_ids).await?;

    let enriched: Vec<Value> = events
        .iter()
        .map(|e| {
            let stats = interactions.get(&e.id);
            let mut obj = serde_json::to_value(e).unwrap();
            if let Some(map) = obj.as_object_mut() {
                map.insert("reactions".into(), json!(stats.map_or(0, |s| s.reactions)));
                map.insert("replies".into(), json!(stats.map_or(0, |s| s.replies)));
                map.insert("reposts".into(), json!(stats.map_or(0, |s| s.reposts)));
                map.insert("zap_sats".into(), json!(stats.map_or(0, |s| s.zap_sats)));
            }
            obj
        })
        .collect();

    Ok(Json(json!({
        "events": enriched,
        "count": enriched.len(),
        "total": total,
    })))
}

/// Get a single event by ID.
pub async fn get_event_by_id(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    match state.repo.get_event_by_id(&id).await? {
        Some(event) => Ok(Json(serde_json::to_value(event).unwrap())),
        None => Err(AppError::NotFound("event not found".into())),
    }
}

/// Frontend-optimized note detail: single SQL round-trip returns event, thread refs,
/// interaction stats, replies, and profile metadata for all involved pubkeys.
pub async fn get_note_detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<ThreadQuery>,
) -> Result<Json<Value>, AppError> {
    let limit = q.limit.unwrap_or(50).min(200);
    match state.repo.get_note_detail(&id, limit).await? {
        Some(detail) => Ok(Json(detail)),
        None => Err(AppError::NotFound("event not found".into())),
    }
}

/// Get full thread context for an event: parent/root refs, replies, reactions, reposts, zaps.
pub async fn get_event_thread(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<ThreadQuery>,
) -> Result<Json<Value>, AppError> {
    let limit = q.limit.unwrap_or(50).min(500);
    match state.repo.get_thread(&id, limit).await? {
        Some(thread) => Ok(Json(serde_json::to_value(thread).unwrap())),
        None => Err(AppError::NotFound("event not found".into())),
    }
}

/// Get interaction counts for an event (lightweight, no full events returned).
pub async fn get_event_interactions(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let interactions = state.repo.get_interactions(&id).await?;
    Ok(Json(serde_json::to_value(interactions).unwrap()))
}

/// Get events of a specific ref_type that reference the given event.
pub async fn get_event_refs(
    State(state): State<AppState>,
    Path((id, ref_type)): Path<(String, String)>,
    Query(q): Query<ThreadQuery>,
) -> Result<Json<Value>, AppError> {
    let valid_types = ["reply", "reaction", "repost", "zap", "mention", "root"];
    if !valid_types.contains(&ref_type.as_str()) {
        return Err(AppError::Internal(format!("invalid ref_type: {ref_type}")));
    }
    let limit = q.limit.unwrap_or(50).min(500);
    let events = state
        .repo
        .get_referencing_events(&id, &ref_type, limit)
        .await?;
    Ok(Json(json!({
        "events": events,
        "count": events.len(),
        "ref_type": ref_type,
    })))
}

/// Return follows/followers summary for a pubkey.
pub async fn get_social_graph(
    State(state): State<AppState>,
    Path(pubkey): Path<String>,
    Query(q): Query<SocialQuery>,
) -> Result<Json<SocialGraphResponse>, AppError> {
    let follows_limit = clamp_limit(q.follows_limit);
    let followers_limit = clamp_limit(q.followers_limit);
    let follows_offset = q.follows_offset.unwrap_or(0).max(0);
    let followers_offset = q.followers_offset.unwrap_or(0).max(0);

    let (follows_count, followers_count) = state.repo.follow_counts(&pubkey).await?;
    
    let follows = state
        .repo
        .list_follows(&pubkey, follows_limit, follows_offset)
        .await?;
    let followers = state
        .repo
        .list_followers(&pubkey, followers_limit, followers_offset)
        .await?;

    Ok(Json(SocialGraphResponse {
        pubkey,
        follows: SocialListResponse {
            count: follows_count,
            pubkeys: follows,
        },
        followers: SocialListResponse {
            count: followers_count,
            pubkeys: followers,
        },
    }))
}

pub async fn get_profiles_metadata(
    State(state): State<AppState>,
    Json(payload): Json<ProfilesMetadataRequest>,
) -> Result<Json<ProfilesMetadataResponse>, AppError> {
    if payload.pubkeys.is_empty() {
        return Ok(Json(ProfilesMetadataResponse { profiles: vec![] }));
    }
    if payload.pubkeys.len() > 500 {
        return Err(AppError::BadRequest(
            "maximum of 500 pubkeys are allowed per request".into(),
        ));
    }

    let mut ordered_pubkeys = Vec::with_capacity(payload.pubkeys.len());
    let mut unique_pubkeys = Vec::new();
    let mut seen = HashSet::new();

    for raw in payload.pubkeys.iter() {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let normalized = normalize_pubkey(trimmed)?;
        ordered_pubkeys.push(normalized.clone());
        if seen.insert(normalized.clone()) {
            unique_pubkeys.push(normalized);
        }
    }

    if ordered_pubkeys.is_empty() {
        return Err(AppError::BadRequest("no valid pubkeys provided".into()));
    }

    // Deterministic cache key from sorted pubkeys
    let mut sorted_for_hash = ordered_pubkeys.clone();
    sorted_for_hash.sort();
    let sorted_joined = sorted_for_hash.join(",");
    let hash = format!("{:x}", Sha256::digest(sorted_joined.as_bytes()));
    let cache_key = format!("profiles:metadata:{hash}");

    if let Some(cached) = state.cache.get_json(&cache_key).await {
        if let Ok(val) = serde_json::from_str::<ProfilesMetadataResponse>(&cached) {
            return Ok(Json(val));
        }
    }

    let rows = state.repo.latest_profile_metadata(&unique_pubkeys).await?;
    let mut metadata_map: HashMap<String, Value> = HashMap::new();
    for row in rows {
        match serde_json::from_str::<Value>(&row.content) {
            Ok(value) => {
                metadata_map.insert(row.pubkey.clone(), value);
            }
            Err(error) => {
                warn!(pubkey = %row.pubkey, %error, "failed to parse metadata content");
            }
        }
    }

    let profiles = ordered_pubkeys
        .into_iter()
        .map(|pubkey| build_profile_entry(&pubkey, metadata_map.get(&pubkey)))
        .collect();

    let response = ProfilesMetadataResponse { profiles };

    if let Ok(json_str) = serde_json::to_string(&response) {
        state.cache.set_json(&cache_key, &json_str, 300).await;
    }

    Ok(Json(response))
}

/// Unified trending endpoint: GET /v1/notes/top?metric=reactions|replies|reposts|zaps&range=today|7d|30d|1y|all
///
/// Aggressively cached in Redis — TTL scales with range (90s for today, 1h for all-time).
pub async fn get_top_notes_unified(
    State(state): State<AppState>,
    Query(q): Query<TopNotesQuery>,
) -> Result<Json<Value>, AppError> {
    let ref_type = match q.metric.as_deref().unwrap_or("reactions") {
        "reactions" => "reaction",
        "replies" => "reply",
        "reposts" => "repost",
        "zaps" => "zap",
        other => {
            return Err(AppError::BadRequest(format!(
                "invalid metric: {other}. Use: reactions, replies, reposts, zaps"
            )))
        }
    };

    let metric = q.metric.as_deref().unwrap_or("reactions").to_string();
    let range = q.range.as_deref().unwrap_or("today").to_string();

    let limit = clamp_listing_limit(q.limit);
    let offset = clamp_offset(q.offset);

    // ── Redis cache check ──────────────────────────────────────────
    if let Some(cached) = state
        .cache
        .get_trending(&metric, &range, limit, offset)
        .await
    {
        if let Ok(val) = serde_json::from_str::<Value>(&cached) {
            return Ok(Json(val));
        }
    }

    // ── Cache miss — compute ───────────────────────────────────────
    let since: Option<i64> = match range.as_str() {
        "today" => Some(Utc::now().timestamp() - 86_400),
        "7d" => Some(Utc::now().timestamp() - 7 * 86_400),
        "30d" => Some(Utc::now().timestamp() - 30 * 86_400),
        "1y" => Some(Utc::now().timestamp() - 365 * 86_400),
        "all" => None,
        other => {
            return Err(AppError::BadRequest(format!(
                "invalid range: {other}. Use: today, 7d, 30d, 1y, all"
            )))
        }
    };

    let (ranked, profile_rows) = state
        .repo
        .top_notes_unified(ref_type, since, limit, offset)
        .await?;

    let profiles: HashMap<String, Value> = profile_rows
        .into_iter()
        .filter_map(|row| {
            serde_json::from_str::<Value>(&row.content).ok().map(|v| {
                let entry = json!({
                    "name": v.get("name").and_then(|n| n.as_str()).filter(|s| !s.trim().is_empty()),
                    "display_name": v.get("display_name").or_else(|| v.get("displayName")).and_then(|n| n.as_str()).filter(|s| !s.trim().is_empty()),
                    "picture": v.get("picture").or_else(|| v.get("image")).and_then(|n| n.as_str()).filter(|s| !s.trim().is_empty()),
                    "nip05": v.get("nip05").and_then(|n| n.as_str()).filter(|s| !s.trim().is_empty()),
                });
                (row.pubkey.clone(), entry)
            })
        })
        .collect();

    let notes: Vec<Value> = ranked
        .into_iter()
        .map(|entry| {
            json!({
                "count": entry.count,
                "total_sats": entry.total_sats,
                "reactions": entry.reactions,
                "replies": entry.replies,
                "reposts": entry.reposts,
                "zap_sats": entry.zap_sats,
                "event": entry.event,
            })
        })
        .collect();

    let response = json!({
        "metric": metric,
        "range": range,
        "notes": notes,
        "profiles": profiles,
    });

    // ── Write to Redis cache ───────────────────────────────────────
    if let Ok(json_str) = serde_json::to_string(&response) {
        state
            .cache
            .set_trending(&metric, &range, limit, offset, &json_str)
            .await;
    }

    Ok(Json(response))
}

/// Get trending notes with composite engagement score.
pub async fn get_trending_notes(
    State(state): State<AppState>,
    Query(q): Query<ListingQuery>,
) -> Result<Json<Value>, AppError> {
    let limit = clamp_listing_limit(q.limit);
    let offset = clamp_offset(q.offset);

    let cache_key = format!("home:trending:{limit}:{offset}");
    if let Some(cached) = state.cache.get_json(&cache_key).await {
        if let Ok(val) = serde_json::from_str::<Value>(&cached) {
            return Ok(Json(val));
        }
    }

    let notes = state.repo.trending_notes(limit, offset).await?;
    let response = json!({ "notes": notes });

    if let Ok(json_str) = serde_json::to_string(&response) {
        state.cache.set_json(&cache_key, &json_str, 300).await;
    }

    Ok(Json(response))
}

/// Get new users (first seen in last 24h).
pub async fn get_new_users(
    State(state): State<AppState>,
    Query(q): Query<ListingQuery>,
) -> Result<Json<Value>, AppError> {
    let limit = clamp_listing_limit(q.limit);
    let offset = clamp_offset(q.offset);

    let cache_key = format!("home:new_users:{limit}:{offset}");
    if let Some(cached) = state.cache.get_json(&cache_key).await {
        if let Ok(val) = serde_json::from_str::<Value>(&cached) {
            return Ok(Json(val));
        }
    }

    let users = state.repo.new_users(limit, offset).await?;
    let response = json!({ "users": users });

    if let Ok(json_str) = serde_json::to_string(&response) {
        state.cache.set_json(&cache_key, &json_str, 300).await;
    }

    Ok(Json(response))
}

/// Get trending users by new follower count (last 24h).
pub async fn get_trending_users(
    State(state): State<AppState>,
    Query(q): Query<ListingQuery>,
) -> Result<Json<Value>, AppError> {
    let limit = clamp_listing_limit(q.limit);
    let offset = clamp_offset(q.offset);

    let cache_key = format!("home:trending_users:{limit}:{offset}");
    if let Some(cached) = state.cache.get_json(&cache_key).await {
        if let Ok(val) = serde_json::from_str::<Value>(&cached) {
            return Ok(Json(val));
        }
    }

    let users = state.repo.trending_users(limit, offset).await?;
    let response = json!({ "users": users });

    if let Ok(json_str) = serde_json::to_string(&response) {
        state.cache.set_json(&cache_key, &json_str, 86_400).await;
    }

    Ok(Json(response))
}

/// Top zappers by sats sent or received in the specified timeframe.
pub async fn get_top_zappers(
    State(state): State<AppState>,
    Query(q): Query<TopZappersQuery>,
) -> Result<Json<Value>, AppError> {
    let direction = q.direction.as_deref().unwrap_or("received");
    if direction != "sent" && direction != "received" {
        return Err(AppError::BadRequest(
            "direction must be 'sent' or 'received'".into(),
        ));
    }
    let range = q.range.as_deref().unwrap_or("7d");
    let limit = clamp_listing_limit(q.limit);
    let offset = clamp_offset(q.offset);

    let cache_key = format!("home:zappers:{direction}:{range}:{limit}:{offset}");
    if let Some(cached) = state.cache.get_json(&cache_key).await {
        if let Ok(val) = serde_json::from_str::<Value>(&cached) {
            return Ok(Json(val));
        }
    }

    let zappers = state.repo.top_zappers(direction, range, limit, offset).await?;
    let response = json!({
        "direction": direction,
        "range": range,
        "zappers": zappers,
    });

    if let Ok(json_str) = serde_json::to_string(&response) {
        // Calculate TTL based on range
        let ttl = match range {
            "today" => 300,    // 5 min
            "7d" => 1800,      // 30 min
            "30d" => 3600,     // 1 hour
            "all" => 86400,    // 1 day
            _ => 1800,         // default to 30 min
        };
        state.cache.set_json(&cache_key, &json_str, ttl).await;
    }

    Ok(Json(response))
}

/// Top posters: authors ranked by number of kind=1 notes published in the timeframe.
pub async fn get_top_posters(
    State(state): State<AppState>,
    Query(q): Query<AnalyticsLeaderboardQuery>,
) -> Result<Json<Value>, AppError> {
    let range = q.range.as_deref().unwrap_or("7d");
    let limit = clamp_listing_limit(q.limit);
    let offset = clamp_offset(q.offset);

    let cache_key = format!("analytics:top_posters:{range}:{limit}:{offset}");
    if let Some(cached) = state.cache.get_json(&cache_key).await {
        if let Ok(val) = serde_json::from_str::<Value>(&cached) {
            return Ok(Json(val));
        }
    }

    let authors = state.repo.top_posters(range, limit, offset).await?;
    let response = json!({
        "range": range,
        "authors": authors,
    });

    if let Ok(json_str) = serde_json::to_string(&response) {
        let ttl = match range {
            "today" => 300,    // 5 min
            "7d" => 1800,      // 30 min
            "30d" => 3600,     // 1 hour
            "all" => 86400,    // 1 day
            _ => 1800,         // default to 30 min
        };
        state.cache.set_json(&cache_key, &json_str, ttl).await;
    }

    Ok(Json(response))
}

/// Most liked authors: authors whose notes received the most reactions (kind=7) in the timeframe.
pub async fn get_most_liked_authors(
    State(state): State<AppState>,
    Query(q): Query<AnalyticsLeaderboardQuery>,
) -> Result<Json<Value>, AppError> {
    let range = q.range.as_deref().unwrap_or("7d");
    let limit = clamp_listing_limit(q.limit);
    let offset = clamp_offset(q.offset);

    let cache_key = format!("analytics:most_liked:{range}:{limit}:{offset}");
    if let Some(cached) = state.cache.get_json(&cache_key).await {
        if let Ok(val) = serde_json::from_str::<Value>(&cached) {
            return Ok(Json(val));
        }
    }

    let authors = state.repo.most_liked_authors(range, limit, offset).await?;
    let response = json!({
        "range": range,
        "authors": authors,
    });

    if let Ok(json_str) = serde_json::to_string(&response) {
        let ttl = match range {
            "today" => 300,    // 5 min
            "7d" => 1800,      // 30 min
            "30d" => 3600,     // 1 hour
            "all" => 86400,    // 1 day
            _ => 1800,         // default to 30 min
        };
        state.cache.set_json(&cache_key, &json_str, ttl).await;
    }

    Ok(Json(response))
}

/// Most shared authors: authors whose notes received the most reposts (kind=6) in the timeframe.
pub async fn get_most_shared_authors(
    State(state): State<AppState>,
    Query(q): Query<AnalyticsLeaderboardQuery>,
) -> Result<Json<Value>, AppError> {
    let range = q.range.as_deref().unwrap_or("7d");
    let limit = clamp_listing_limit(q.limit);
    let offset = clamp_offset(q.offset);

    let cache_key = format!("analytics:most_shared:{range}:{limit}:{offset}");
    if let Some(cached) = state.cache.get_json(&cache_key).await {
        if let Ok(val) = serde_json::from_str::<Value>(&cached) {
            return Ok(Json(val));
        }
    }

    let authors = state.repo.most_shared_authors(range, limit, offset).await?;
    let response = json!({
        "range": range,
        "authors": authors,
    });

    if let Ok(json_str) = serde_json::to_string(&response) {
        let ttl = match range {
            "today" => 300,    // 5 min
            "7d" => 1800,      // 30 min
            "30d" => 3600,     // 1 hour
            "all" => 86400,    // 1 day
            _ => 1800,         // default to 30 min
        };
        state.cache.set_json(&cache_key, &json_str, ttl).await;
    }

    Ok(Json(response))
}

/// Get daily network stats (DAU, total sats, daily posts).
///
/// DAU and daily posts are served from Redis (HyperLogLog + counter) for O(1)
/// lookups. Zap sats use a lightweight indexed DB query (~5ms). Falls back to
/// full DB query on cold start (first request after restart before any events
/// have been ingested).
pub async fn get_daily_stats(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let cache_key = "home:daily_stats";
    if let Some(cached) = state.cache.get_json(cache_key).await {
        if let Ok(val) = serde_json::from_str::<Value>(&cached) {
            return Ok(Json(val));
        }
    }

    let stats = if let Some((dau, daily_posts)) = state.cache.get_daily_dau_posts().await {
        // Fast path: DAU + posts from Redis, only zap sats from DB (indexed, ~5ms).
        let total_sats = state.repo.daily_zap_sats().await.unwrap_or(0);
        crate::db::models::DailyStats {
            daily_active_users: dau,
            total_sats_sent: total_sats,
            daily_posts,
        }
    } else {
        // Cold start: full DB fallback (slow, but only happens once after restart).
        state.repo.daily_stats().await?
    };

    let response = serde_json::to_value(&stats).unwrap();

    if let Ok(json_str) = serde_json::to_string(&response) {
        // Cache for 60s — data is live from Redis anyway, this just prevents
        // redundant DB zap queries under load.
        state.cache.set_json(cache_key, &json_str, 60).await;
    }

    Ok(Json(response))
}



#[derive(Debug, Deserialize)]
pub struct TopNotesQuery {
    /// "reactions", "replies", "reposts", "zaps" (default: "reactions")
    pub metric: Option<String>,
    /// "today", "7d", "30d", "1y", "all" (default: "today")
    pub range: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, serde::Deserialize)]
pub struct ThreadQuery {
    pub limit: Option<i64>,
}

#[derive(Debug, serde::Deserialize)]
pub struct SocialQuery {
    pub follows_limit: Option<i64>,
    pub followers_limit: Option<i64>,
    pub follows_offset: Option<i64>,
    pub followers_offset: Option<i64>,
}

#[derive(Debug, serde::Deserialize)]
pub struct ListingQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, serde::Deserialize)]
pub struct ProfileTabQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub sort: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
pub struct ProfileZapsQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub sort: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
pub struct TopZappersQuery {
    pub direction: Option<String>,
    pub range: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, serde::Deserialize)]
pub struct AnalyticsLeaderboardQuery {
    pub range: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct SocialListResponse {
    pub count: i64,
    pub pubkeys: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct SocialGraphResponse {
    pub pubkey: String,
    pub follows: SocialListResponse,
    pub followers: SocialListResponse,
}

#[derive(Debug, Deserialize)]
pub struct ProfilesMetadataRequest {
    pub pubkeys: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProfileMetadataEntry {
    pub pubkey: String,
    pub display_name: Option<String>,
    pub name: Option<String>,
    pub preferred_name: Option<String>,
    pub picture: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProfilesMetadataResponse {
    pub profiles: Vec<ProfileMetadataEntry>,
}

fn clamp_limit(value: Option<i64>) -> i64 {
    value.unwrap_or(100).clamp(1, 500)
}

fn clamp_listing_limit(value: Option<i64>) -> i64 {
    value.unwrap_or(100).clamp(1, 100)
}

fn clamp_profile_tab_limit(value: Option<i64>) -> i64 {
    value.unwrap_or(20).clamp(1, 100)
}

fn clamp_offset(value: Option<i64>) -> i64 {
    value.unwrap_or(0).max(0)
}

fn build_profile_entry(pubkey: &str, metadata: Option<&Value>) -> ProfileMetadataEntry {
    let display_name =
        metadata.and_then(|value| get_string(value, &["display_name", "displayName"]));
    let name = metadata.and_then(|value| get_string(value, &["name", "username"]));
    let picture = metadata.and_then(|value| get_string(value, &["picture", "image"]));

    let preferred_name = display_name.clone().or_else(|| name.clone());

    ProfileMetadataEntry {
        pubkey: pubkey.to_string(),
        display_name,
        name,
        preferred_name,
        picture,
    }
}

fn get_string(value: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(raw) = value.get(key).and_then(|v| v.as_str()) {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub q: String,
    /// "profiles", "notes", or "all" (default "all")
    #[serde(rename = "type", default = "default_search_type")]
    pub search_type: String,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

fn default_search_type() -> String {
    "all".into()
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profiles: Option<Vec<ProfileSearchResult>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<Vec<NoteSearchResult>>,
}

/// Full search endpoint: `GET /v1/search?q=<query>&type=profiles|notes|all`
///
/// 1. Attempts to decode the query as a Nostr entity (npub, nprofile, nevent, note1, hex).
///    If successful, returns a `resolved` object for direct navigation.
/// 2. Otherwise performs ranked search across profiles and/or notes.
pub async fn search(
    State(state): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<SearchResponse>, AppError> {
    let query = q.q.trim().to_string();
    if query.is_empty() {
        return Err(AppError::BadRequest(
            "query parameter 'q' is required".into(),
        ));
    }

    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let offset = q.offset.unwrap_or(0).max(0);

    // Try entity resolution first
    if let Some(resolved) = resolve_entity(&query, &state).await? {
        return Ok(Json(SearchResponse {
            query,
            resolved: Some(resolved),
            profiles: None,
            notes: None,
        }));
    }

    let include_profiles = q.search_type == "all" || q.search_type == "profiles";
    let include_notes = q.search_type == "all" || q.search_type == "notes";

    let profiles = if include_profiles {
        Some(
            state
                .profile_search_cache
                .search_profiles(&query, limit, offset)
                .await,
        )
    } else {
        None
    };

    let notes = if include_notes {
        Some(state.repo.search_notes(&query, limit, offset).await?)
    } else {
        None
    };

    Ok(Json(SearchResponse {
        query,
        resolved: None,
        profiles,
        notes,
    }))
}

#[derive(Debug, Deserialize)]
pub struct SuggestQuery {
    pub q: String,
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct SuggestResponse {
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved: Option<Value>,
    pub suggestions: Vec<ProfileSearchResult>,
}

/// Autocomplete endpoint: `GET /v1/search/suggest?q=<query>&limit=5`
///
/// Lightweight, Redis-cached endpoint for search-as-you-type.
/// Returns profile suggestions ranked by prefix match quality and follower count.
/// Also detects and resolves Nostr entities (npub, nprofile, nevent, note1).
pub async fn search_suggest(
    State(state): State<AppState>,
    Query(q): Query<SuggestQuery>,
) -> Result<Json<SuggestResponse>, AppError> {
    let query = q.q.trim().to_string();
    if query.len() < 2 {
        return Err(AppError::BadRequest(
            "query must be at least 2 characters".into(),
        ));
    }

    let limit = q.limit.unwrap_or(5).clamp(1, 10);

    // For entity-like inputs, try resolution instead of text search
    if nip19::looks_like_entity(&query) {
        if let Some(resolved) = resolve_entity(&query, &state).await? {
            return Ok(Json(SuggestResponse {
                query,
                resolved: Some(resolved),
                suggestions: vec![],
            }));
        }
    }

    // In-memory search — no DB or Redis needed
    let suggestions = state
        .profile_search_cache
        .suggest_profiles(&query, limit)
        .await;

    Ok(Json(SuggestResponse {
        query,
        resolved: None,
        suggestions,
    }))
}

/// Try to resolve a query string as a Nostr entity.
async fn resolve_entity(input: &str, state: &AppState) -> Result<Option<Value>, AppError> {
    // Check for NIP-19 encoded entities
    if let Some(entity) = nip19::decode(input) {
        match &entity {
            nip19::NostrEntity::Event { id, relays, .. } => {
                // Try to fetch the event if not in DB, using relay hints
                if state.repo.get_event_by_id(id).await?.is_none() {
                    if let Ok(Some(_)) = state.fetcher.fetch_event_by_id(id, relays).await {
                        tracing::info!(event_id = %id, "fetched event from relay hints");
                    }
                }
            }
            nip19::NostrEntity::Profile { pubkey, relays } => {
                // Try to fetch profile metadata if not in DB, using relay hints
                let profile_rows = state.repo.latest_profile_metadata(&[pubkey.clone()]).await?;
                if profile_rows.is_empty() {
                    if let Ok(Some(_)) = state.fetcher.fetch_profile_metadata(pubkey, relays).await {
                        tracing::info!(pubkey = %pubkey, "fetched profile from relay hints");
                    }
                }
            }
        }
        return Ok(Some(serde_json::to_value(entity).unwrap()));
    }

    // Check for raw 64-char hex
    if nip19::is_hex64(input) {
        if let Some((entity_type, id)) = state.repo.resolve_hex(input).await? {
            let resolved = match entity_type {
                "event" => json!({ "type": "event", "id": id }),
                _ => json!({ "type": "profile", "pubkey": id }),
            };
            return Ok(Some(resolved));
        }
        // Raw hex is ambiguous (could be pubkey or event) and has no relay hints,
        // so we don't trigger on-demand fetching here to avoid abuse.
    }

    Ok(None)
}

/// GET /v1/crawler/stats — crawler queue statistics.
pub async fn get_crawler_stats(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, AppError> {
    match &state.crawl_queue {
        Some(queue) => {
            let stats = queue.stats().await?;
            Ok(Json(serde_json::to_value(stats).unwrap()))
        }
        None => Ok(Json(serde_json::json!({ "enabled": false }))),
    }
}

// ---------------------------------------------------------------------------
// Advanced Note Search
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct AdvancedNoteSearchQuery {
    pub q: Option<String>,
    pub exclude: Option<String>,
    pub author: Option<String>,
    pub reply_to: Option<String>,
    pub order: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// Advanced note search: `GET /v1/notes/search?q=bitcoin&exclude=scam&author=npub1...&reply_to=npub1...&order=engagement&limit=20&offset=0`
pub async fn advanced_note_search(
    State(state): State<AppState>,
    Query(q): Query<AdvancedNoteSearchQuery>,
) -> Result<Json<Value>, AppError> {
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let offset = q.offset.unwrap_or(0).max(0);

    let order = match q.order.as_deref().unwrap_or("newest") {
        "newest" | "oldest" | "engagement" => q.order.as_deref().unwrap_or("newest"),
        other => {
            return Err(AppError::BadRequest(format!(
                "invalid order: {other}. Use: newest, oldest, engagement"
            )))
        }
    };

    // Normalize pubkeys
    let author = match &q.author {
        Some(a) => Some(normalize_pubkey(a)?),
        None => None,
    };
    let reply_to = match &q.reply_to {
        Some(r) => Some(normalize_pubkey(r)?),
        None => None,
    };

    let (entries, total, profile_rows) = state
        .repo
        .advanced_search_notes(
            q.q.as_deref(),
            q.exclude.as_deref(),
            author.as_deref(),
            reply_to.as_deref(),
            order,
            limit,
            offset,
        )
        .await?;

    let profiles: HashMap<String, Value> = profile_rows
        .into_iter()
        .filter_map(|row| {
            serde_json::from_str::<Value>(&row.content).ok().map(|v| {
                let entry = json!({
                    "name": v.get("name").and_then(|n| n.as_str()).filter(|s| !s.trim().is_empty()),
                    "display_name": v.get("display_name").or_else(|| v.get("displayName")).and_then(|n| n.as_str()).filter(|s| !s.trim().is_empty()),
                    "picture": v.get("picture").or_else(|| v.get("image")).and_then(|n| n.as_str()).filter(|s| !s.trim().is_empty()),
                    "nip05": v.get("nip05").and_then(|n| n.as_str()).filter(|s| !s.trim().is_empty()),
                });
                (row.pubkey.clone(), entry)
            })
        })
        .collect();

    let notes: Vec<Value> = entries
        .into_iter()
        .map(|e| {
            json!({
                "event": e.event,
                "reactions": e.reactions,
                "replies": e.replies,
                "reposts": e.reposts,
                "zap_sats": e.zap_sats,
            })
        })
        .collect();

    Ok(Json(json!({
        "notes": notes,
        "total": total,
        "profiles": profiles,
    })))
}


// ─── Profile Tabs ─────────────────────────────────────────────

/// Profile notes (non-replies): GET /v1/profiles/{pubkey}/notes
pub async fn get_profile_notes(
    State(state): State<AppState>,
    Path(pubkey): Path<String>,
    Query(q): Query<ProfileTabQuery>,
) -> Result<Json<Value>, AppError> {
    let pubkey = normalize_pubkey(&pubkey)?;
    let limit = clamp_profile_tab_limit(q.limit);
    let offset = clamp_offset(q.offset);
    let sort = q.sort.as_deref().unwrap_or("recent");

    let cache_key = format!("profile:notes:{pubkey}:{limit}:{offset}:{sort}");
    if let Some(cached) = state.cache.get_json(&cache_key).await {
        if let Ok(val) = serde_json::from_str::<Value>(&cached) {
            return Ok(Json(val));
        }
    }

    let (events, total) = state.repo.profile_notes(&pubkey, limit, offset, sort).await?;

    let event_ids: Vec<String> = events.iter().map(|e| e.id.clone()).collect();
    let interactions = state.repo.batch_get_interactions(&event_ids).await?;

    let mut profile_pubkeys: HashSet<String> = events.iter().map(|e| e.pubkey.clone()).collect();
    profile_pubkeys.insert(pubkey.clone());
    profile_pubkeys.extend(collect_mentioned_pubkeys(&events));

    let profile_rows = state
        .repo
        .latest_profile_metadata(&profile_pubkeys.into_iter().collect::<Vec<_>>())
        .await?;
    let profiles = build_profiles_map(profile_rows);

    let enriched = enrich_events_with_interactions(&events, &interactions);

    let response = json!({
        "events": enriched,
        "total": total,
        "profiles": profiles,
    });

    if let Ok(json_str) = serde_json::to_string(&response) {
        state.cache.set_json(&cache_key, &json_str, 60).await;
    }

    Ok(Json(response))
}

/// Profile replies: GET /v1/profiles/{pubkey}/replies
pub async fn get_profile_replies(
    State(state): State<AppState>,
    Path(pubkey): Path<String>,
    Query(q): Query<ProfileTabQuery>,
) -> Result<Json<Value>, AppError> {
    let pubkey = normalize_pubkey(&pubkey)?;
    let limit = clamp_profile_tab_limit(q.limit);
    let offset = clamp_offset(q.offset);
    let sort = q.sort.as_deref().unwrap_or("recent");

    let cache_key = format!("profile:replies:{pubkey}:{limit}:{offset}:{sort}");
    if let Some(cached) = state.cache.get_json(&cache_key).await {
        if let Ok(val) = serde_json::from_str::<Value>(&cached) {
            return Ok(Json(val));
        }
    }

    let (events, total) = state.repo.profile_replies(&pubkey, limit, offset, sort).await?;

    let event_ids: Vec<String> = events.iter().map(|e| e.id.clone()).collect();
    let interactions = state.repo.batch_get_interactions(&event_ids).await?;

    let mut profile_pubkeys: HashSet<String> = events.iter().map(|e| e.pubkey.clone()).collect();
    profile_pubkeys.insert(pubkey.clone());
    profile_pubkeys.extend(collect_mentioned_pubkeys(&events));

    let profile_rows = state
        .repo
        .latest_profile_metadata(&profile_pubkeys.into_iter().collect::<Vec<_>>())
        .await?;
    let profiles = build_profiles_map(profile_rows);

    let enriched = enrich_events_with_interactions(&events, &interactions);

    let response = json!({
        "events": enriched,
        "total": total,
        "profiles": profiles,
    });

    if let Ok(json_str) = serde_json::to_string(&response) {
        state.cache.set_json(&cache_key, &json_str, 60).await;
    }

    Ok(Json(response))
}

/// Zaps sent by profile: GET /v1/profiles/{pubkey}/zaps/sent
pub async fn get_profile_zaps_sent(
    State(state): State<AppState>,
    Path(pubkey): Path<String>,
    Query(q): Query<ProfileZapsQuery>,
) -> Result<Json<Value>, AppError> {
    let pubkey = normalize_pubkey(&pubkey)?;
    let limit = clamp_profile_tab_limit(q.limit);
    let offset = clamp_offset(q.offset);
    let sort = q.sort.as_deref().unwrap_or("recent");

    let cache_key = format!("profile:zaps_sent:{pubkey}:{limit}:{offset}:{sort}");
    if let Some(cached) = state.cache.get_json(&cache_key).await {
        if let Ok(val) = serde_json::from_str::<Value>(&cached) {
            return Ok(Json(val));
        }
    }

    let (entries, total, profile_rows) = state.repo.profile_zaps_sent(&pubkey, limit, offset, sort).await?;
    let profiles = build_profiles_map(profile_rows);

    let zaps: Vec<Value> = entries
        .iter()
        .map(|e| {
            json!({
                "event": e.event,
                "amount_sats": e.amount_sats,
                "recipient": e.counterparty,
                "zapped_event_id": e.zapped_event_id,
            })
        })
        .collect();

    let response = json!({
        "zaps": zaps,
        "total": total,
        "profiles": profiles,
    });

    if let Ok(json_str) = serde_json::to_string(&response) {
        state.cache.set_json(&cache_key, &json_str, 120).await;
    }

    Ok(Json(response))
}

/// Zaps received by profile: GET /v1/profiles/{pubkey}/zaps/received
pub async fn get_profile_zaps_received(
    State(state): State<AppState>,
    Path(pubkey): Path<String>,
    Query(q): Query<ProfileZapsQuery>,
) -> Result<Json<Value>, AppError> {
    let pubkey = normalize_pubkey(&pubkey)?;
    let limit = clamp_profile_tab_limit(q.limit);
    let offset = clamp_offset(q.offset);
    let sort = q.sort.as_deref().unwrap_or("recent");

    let cache_key = format!("profile:zaps_recv:{pubkey}:{limit}:{offset}:{sort}");
    if let Some(cached) = state.cache.get_json(&cache_key).await {
        if let Ok(val) = serde_json::from_str::<Value>(&cached) {
            return Ok(Json(val));
        }
    }

    let (entries, total, profile_rows) = state
        .repo
        .profile_zaps_received(&pubkey, limit, offset, sort)
        .await?;
    let profiles = build_profiles_map(profile_rows);

    let zaps: Vec<Value> = entries
        .iter()
        .map(|e| {
            json!({
                "event": e.event,
                "amount_sats": e.amount_sats,
                "sender": e.counterparty,
                "zapped_event_id": e.zapped_event_id,
            })
        })
        .collect();

    let response = json!({
        "zaps": zaps,
        "total": total,
        "profiles": profiles,
    });

    if let Ok(json_str) = serde_json::to_string(&response) {
        state.cache.set_json(&cache_key, &json_str, 120).await;
    }

    Ok(Json(response))
}

/// Aggregate zap stats: GET /v1/profiles/{pubkey}/zap-stats
pub async fn get_profile_zap_stats(
    State(state): State<AppState>,
    Path(pubkey): Path<String>,
) -> Result<Json<Value>, AppError> {
    let pubkey = normalize_pubkey(&pubkey)?;

    let cache_key = format!("profile:zap_stats:{pubkey}");
    if let Some(cached) = state.cache.get_json(&cache_key).await {
        if let Ok(val) = serde_json::from_str::<Value>(&cached) {
            return Ok(Json(val));
        }
    }

    let stats = state.repo.profile_zap_stats(&pubkey).await?;
    let response = serde_json::to_value(&stats).unwrap();

    if let Ok(json_str) = serde_json::to_string(&response) {
        state.cache.set_json(&cache_key, &json_str, 300).await;
    }

    Ok(Json(response))
}

// ─── Shared helpers for profile tabs ──────────────────────────

fn build_profiles_map(
    profile_rows: Vec<crate::db::repository::ProfileRow>,
) -> HashMap<String, Value> {
    profile_rows
        .into_iter()
        .filter_map(|row| {
            serde_json::from_str::<Value>(&row.content).ok().map(|v| {
                let entry = json!({
                    "name": v.get("name").and_then(|n| n.as_str()).filter(|s| !s.trim().is_empty()),
                    "display_name": v.get("display_name").or_else(|| v.get("displayName")).and_then(|n| n.as_str()).filter(|s| !s.trim().is_empty()),
                    "picture": v.get("picture").or_else(|| v.get("image")).and_then(|n| n.as_str()).filter(|s| !s.trim().is_empty()),
                    "nip05": v.get("nip05").and_then(|n| n.as_str()).filter(|s| !s.trim().is_empty()),
                });
                (row.pubkey.clone(), entry)
            })
        })
        .collect()
}

fn enrich_events_with_interactions(
    events: &[crate::db::models::StoredEvent],
    interactions: &HashMap<String, crate::db::models::EventInteractions>,
) -> Vec<Value> {
    events
        .iter()
        .map(|e| {
            let stats = interactions.get(&e.id);
            let mut obj = serde_json::to_value(e).unwrap();
            if let Some(map) = obj.as_object_mut() {
                map.insert("reactions".into(), json!(stats.map_or(0, |s| s.reactions)));
                map.insert("replies".into(), json!(stats.map_or(0, |s| s.replies)));
                map.insert("reposts".into(), json!(stats.map_or(0, |s| s.reposts)));
                map.insert("zap_sats".into(), json!(stats.map_or(0, |s| s.zap_sats)));
            }
            obj
        })
        .collect()
}

fn collect_mentioned_pubkeys(events: &[crate::db::models::StoredEvent]) -> HashSet<String> {
    let mut pubkeys = HashSet::new();

    for event in events {
        for tag in event.tags.0.iter() {
            if tag.len() >= 2 && tag[0] == "p" {
                if let Ok(pubkey) = normalize_pubkey(&tag[1]) {
                    pubkeys.insert(pubkey);
                }
            }
        }
    }

    pubkeys
}

fn normalize_pubkey(input: &str) -> Result<String, AppError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(AppError::BadRequest("empty pubkey".into()));
    }

    if trimmed.to_ascii_lowercase().starts_with("npub") {
        decode_npub(trimmed)
    } else if trimmed.len() == 64 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(trimmed.to_ascii_lowercase())
    } else {
        Err(AppError::BadRequest(format!("invalid pubkey: {trimmed}")))
    }
}

/// Get trending hashtags from the last 24 hours.
pub async fn get_trending_hashtags(
    State(state): State<AppState>,
    Query(q): Query<ListingQuery>,
) -> Result<Json<Value>, AppError> {
    let limit = clamp_listing_limit(q.limit).min(50);
    let offset = clamp_offset(q.offset);

    let cache_key = format!("home:trending_hashtags:{limit}:{offset}");
    if let Some(cached) = state.cache.get_json(&cache_key).await {
        if let Ok(val) = serde_json::from_str::<Value>(&cached) {
            return Ok(Json(val));
        }
    }

    let hashtags = state.repo.trending_hashtags(limit, offset).await?;
    let response = json!({
        "hashtags": hashtags,
    });

    if let Ok(json_str) = serde_json::to_string(&response) {
        state.cache.set_json(&cache_key, &json_str, 600).await;
    }

    Ok(Json(response))
}

/// Get notes tagged with a specific hashtag: `GET /v1/hashtags/:tag/notes?limit=20&offset=0`
pub async fn get_hashtag_notes(
    State(state): State<AppState>,
    Path(tag): Path<String>,
    Query(q): Query<ListingQuery>,
) -> Result<Json<Value>, AppError> {
    let tag = tag.trim().to_lowercase();
    if tag.is_empty() || tag.len() > 100 {
        return Err(AppError::BadRequest("invalid hashtag".into()));
    }

    let limit = clamp_listing_limit(q.limit);
    let offset = clamp_offset(q.offset);

    let cache_key = format!("hashtag:{}:{limit}:{offset}", tag);
    if let Some(cached) = state.cache.get_json(&cache_key).await {
        if let Ok(val) = serde_json::from_str::<Value>(&cached) {
            return Ok(Json(val));
        }
    }

    let (notes, profile_rows) = state.repo.notes_by_hashtag(&tag, limit, offset).await?;

    let profiles: HashMap<String, Value> = profile_rows
        .into_iter()
        .filter_map(|row| {
            serde_json::from_str::<Value>(&row.content).ok().map(|v| {
                let entry = json!({
                    "name": v.get("name").and_then(|n| n.as_str()).filter(|s| !s.trim().is_empty()),
                    "display_name": v.get("display_name").or_else(|| v.get("displayName")).and_then(|n| n.as_str()).filter(|s| !s.trim().is_empty()),
                    "picture": v.get("picture").or_else(|| v.get("image")).and_then(|n| n.as_str()).filter(|s| !s.trim().is_empty()),
                    "nip05": v.get("nip05").and_then(|n| n.as_str()).filter(|s| !s.trim().is_empty()),
                });
                (row.pubkey.clone(), entry)
            })
        })
        .collect();

    let response = json!({
        "hashtag": tag,
        "notes": notes,
        "profiles": profiles,
    });

    if let Ok(json_str) = serde_json::to_string(&response) {
        state.cache.set_json(&cache_key, &json_str, 300).await;
    }

    Ok(Json(response))
}

/// Client leaderboard: `GET /v1/clients/leaderboard?limit=50&offset=0`
///
/// Returns Nostr clients ranked by note count, with distinct user counts.
/// Redis-cached for 10 minutes.
pub async fn get_client_leaderboard(
    State(state): State<AppState>,
    Query(q): Query<ListingQuery>,
) -> Result<Json<Value>, AppError> {
    let limit = clamp_listing_limit(q.limit);
    let offset = clamp_offset(q.offset);

    let cache_key = format!("clients:leaderboard:{limit}:{offset}");
    if let Some(cached) = state.cache.get_json(&cache_key).await {
        if let Ok(val) = serde_json::from_str::<Value>(&cached) {
            return Ok(Json(val));
        }
    }

    let clients = state.repo.client_leaderboard(limit, offset).await?;
    let response = json!({ "clients": clients });

    if let Ok(json_str) = serde_json::to_string(&response) {
        state.cache.set_json(&cache_key, &json_str, 600).await;
    }

    Ok(Json(response))
}

pub async fn get_relay_leaderboard(
    State(state): State<AppState>,
    Query(q): Query<ListingQuery>,
) -> Result<Json<Value>, AppError> {
    let limit = clamp_listing_limit(q.limit);
    let offset = clamp_offset(q.offset);

    let cache_key = format!("relays:leaderboard:{limit}:{offset}");
    if let Some(cached) = state.cache.get_json(&cache_key).await {
        if let Ok(val) = serde_json::from_str::<Value>(&cached) {
            return Ok(Json(val));
        }
    }

    let relays = state.repo.relay_leaderboard(limit, offset).await?;
    let response = json!({ "relays": relays });

    if let Ok(json_str) = serde_json::to_string(&response) {
        // Cache for 30 minutes (heavy query)
        state.cache.set_json(&cache_key, &json_str, 1800).await;
    }

    Ok(Json(response))
}

// ---------------------------------------------------------------------------
// Daily Analytics
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct DailyAnalyticsQuery {
    pub days: Option<i64>,
}

/// GET /v1/analytics/daily?days=30
///
/// Returns daily analytics for the last N days (default 30, max 365).
/// Redis-cached for 24 hours (data is immutable once computed).
pub async fn get_analytics_daily(
    State(state): State<AppState>,
    Query(q): Query<DailyAnalyticsQuery>,
) -> Result<Json<Value>, AppError> {
    let days = q.days.unwrap_or(30).clamp(1, 365);

    let cache_key = format!("analytics:daily:{days}");
    if let Some(cached) = state.cache.get_json(&cache_key).await {
        if let Ok(val) = serde_json::from_str::<Value>(&cached) {
            return Ok(Json(val));
        }
    }

    let today = Utc::now().date_naive();
    let since = today - chrono::Duration::days(days);

    let data = state.repo.get_daily_analytics(since, today).await?;

    let response = json!({
        "data": data,
        "range": {
            "from": since.format("%Y-%m-%d").to_string(),
            "to": today.format("%Y-%m-%d").to_string(),
        }
    });

    if let Ok(json_str) = serde_json::to_string(&response) {
        state.cache.set_json(&cache_key, &json_str, 86400).await;
    }

    Ok(Json(response))
}

fn decode_npub(npub: &str) -> Result<String, AppError> {
    let (hrp, data, _) =
        bech32::decode(npub).map_err(|_| AppError::BadRequest(format!("invalid npub: {npub}")))?;
    if hrp != "npub" {
        return Err(AppError::BadRequest(format!("invalid npub: {npub}")));
    }

    let bytes = Vec::<u8>::from_base32(&data)
        .map_err(|_| AppError::BadRequest(format!("invalid npub: {npub}")))?;
    if bytes.len() != 32 {
        return Err(AppError::BadRequest(format!("invalid npub: {npub}")));
    }

    Ok(hex::encode(bytes))
}
