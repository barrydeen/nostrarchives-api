use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
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
    let events = state.repo.get_referencing_events(&id, &ref_type, limit).await?;
    Ok(Json(json!({
        "events": events,
        "count": events.len(),
        "ref_type": ref_type,
    })))
}

#[derive(Debug, serde::Deserialize)]
pub struct ThreadQuery {
    pub limit: Option<i64>,
}
