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
use crate::db::models::{EventQuery, NoteSearchResult, ProfileSearchResult, StoredEvent};
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
        state.cache.set_json(&cache_key, &json_str, 300).await;
    }

    Ok(Json(response))
}

/// Top zappers by sats sent or received in last 24h.
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
    let limit = clamp_listing_limit(q.limit);
    let offset = clamp_offset(q.offset);

    let cache_key = format!("home:zappers:{direction}:{limit}:{offset}");
    if let Some(cached) = state.cache.get_json(&cache_key).await {
        if let Ok(val) = serde_json::from_str::<Value>(&cached) {
            return Ok(Json(val));
        }
    }

    let zappers = state.repo.top_zappers(direction, limit, offset).await?;
    let response = json!({
        "direction": direction,
        "zappers": zappers,
    });

    if let Ok(json_str) = serde_json::to_string(&response) {
        state.cache.set_json(&cache_key, &json_str, 300).await;
    }

    Ok(Json(response))
}

/// Get daily network stats (DAU, total sats, daily posts).
pub async fn get_daily_stats(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let cache_key = "home:daily_stats";
    if let Some(cached) = state.cache.get_json(cache_key).await {
        if let Ok(val) = serde_json::from_str::<Value>(&cached) {
            return Ok(Json(val));
        }
    }

    let stats = state.repo.daily_stats().await?;
    let response = serde_json::to_value(stats).unwrap();

    if let Ok(json_str) = serde_json::to_string(&response) {
        state.cache.set_json(cache_key, &json_str, 120).await;
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
pub struct TopZappersQuery {
    pub direction: Option<String>,
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
        Some(state.repo.search_profiles(&query, limit, offset).await?)
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

    // Check Redis cache
    if let Some(cached) = state.cache.get_search_suggest(&query).await {
        if let Ok(suggestions) = serde_json::from_str::<Vec<ProfileSearchResult>>(&cached) {
            return Ok(Json(SuggestResponse {
                query,
                resolved: None,
                suggestions,
            }));
        }
    }

    // Query DB
    let suggestions = state.repo.suggest_profiles(&query, limit).await?;

    // Cache result
    if let Ok(json) = serde_json::to_string(&suggestions) {
        state.cache.set_search_suggest(&query, &json).await;
    }

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
