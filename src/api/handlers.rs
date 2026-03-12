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
