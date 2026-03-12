use std::collections::{HashMap, HashSet};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use bech32::{self, FromBase32};
use chrono::{Duration, Utc};
use hex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::warn;

use super::AppState;
use crate::db::models::{EventQuery, StoredEvent};
use crate::db::repository::EventRepository;
use crate::error::AppError;

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
    let events = state.repo.query_events(&q).await?;
    Ok(Json(json!({
        "events": events,
        "count": events.len(),
    })))
}

/// Get a single event by ID.
pub async fn get_event_by_id(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    match state.repo.get_event_by_id(&id).await? {
        Some(event) => Ok(Json(serde_json::to_value(event).unwrap())),
        None => Err(AppError::Internal("event not found".into())),
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
        None => Err(AppError::Internal("event not found".into())),
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

    Ok(Json(ProfilesMetadataResponse { profiles }))
}

pub async fn get_top_likes(
    State(state): State<AppState>,
    Query(q): Query<ListingQuery>,
) -> Result<Json<RankedNotesResponse>, AppError> {
    ranked_notes(
        state.repo, "reaction", None, "likes", "all_time", q.limit, q.offset,
    )
    .await
}

pub async fn get_top_likes_today(
    State(state): State<AppState>,
    Query(q): Query<ListingQuery>,
) -> Result<Json<RankedNotesResponse>, AppError> {
    let since = Utc::now().timestamp() - Duration::hours(24).num_seconds();
    ranked_notes(
        state.repo,
        "reaction",
        Some(since),
        "likes",
        "today",
        q.limit,
        q.offset,
    )
    .await
}

pub async fn get_top_zaps(
    State(state): State<AppState>,
    Query(q): Query<ListingQuery>,
) -> Result<Json<RankedNotesResponse>, AppError> {
    ranked_notes(
        state.repo, "zap", None, "zaps", "all_time", q.limit, q.offset,
    )
    .await
}

pub async fn get_top_zaps_today(
    State(state): State<AppState>,
    Query(q): Query<ListingQuery>,
) -> Result<Json<RankedNotesResponse>, AppError> {
    let since = Utc::now().timestamp() - Duration::hours(24).num_seconds();
    ranked_notes(
        state.repo,
        "zap",
        Some(since),
        "zaps",
        "today",
        q.limit,
        q.offset,
    )
    .await
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

#[derive(Debug, Serialize)]
pub struct RankedNoteResponse {
    pub count: i64,
    pub event: StoredEvent,
}

#[derive(Debug, Serialize)]
pub struct RankedNotesResponse {
    pub metric: String,
    pub range: String,
    pub notes: Vec<RankedNoteResponse>,
}

#[derive(Debug, Deserialize)]
pub struct ProfilesMetadataRequest {
    pub pubkeys: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ProfileMetadataEntry {
    pub pubkey: String,
    pub display_name: Option<String>,
    pub name: Option<String>,
    pub preferred_name: Option<String>,
    pub picture: Option<String>,
}

#[derive(Debug, Serialize)]
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

async fn ranked_notes(
    repo: EventRepository,
    ref_type: &str,
    since: Option<i64>,
    metric: &str,
    range: &str,
    limit: Option<i64>,
    offset: Option<i64>,
) -> Result<Json<RankedNotesResponse>, AppError> {
    let limit = clamp_listing_limit(limit);
    let offset = clamp_offset(offset);
    let ranked = repo
        .top_notes_by_ref(ref_type, since, limit, offset)
        .await?;

    let notes = ranked
        .into_iter()
        .map(|entry| RankedNoteResponse {
            count: entry.count,
            event: entry.event,
        })
        .collect();

    Ok(Json(RankedNotesResponse {
        metric: metric.into(),
        range: range.into(),
        notes,
    }))
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
