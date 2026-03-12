use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Serialize;
use serde_json::{json, Value};

use super::AppState;
use crate::db::models::EventQuery;
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

fn clamp_limit(value: Option<i64>) -> i64 {
    value.unwrap_or(100).clamp(1, 500)
}
