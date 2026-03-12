use std::net::SocketAddr;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::any;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::broadcast;

use crate::api::AppState;

/// Feed type determines which query backs a feed endpoint.
#[derive(Debug, Clone)]
enum FeedKind {
    /// NIP-50 search relay (existing behavior).
    Search,
    /// Trending notes from the last 24 hours.
    TrendingToday,
}

/// Build the WebSocket relay router.
pub fn router(state: AppState) -> Router {
    Router::new()
        // Existing search relay on root path
        .route("/", any(ws_search_handler))
        // Feed endpoints: /trending/today
        .route("/trending/today", any(ws_trending_today_handler))
        .with_state(state)
}

/// Start the WebSocket relay listener on a separate port.
pub async fn serve(state: AppState, addr: SocketAddr, mut shutdown_rx: broadcast::Receiver<()>) {
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind ws listener");

    tracing::info!(addr = %addr, "websocket relay listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.recv().await;
        })
        .await
        .expect("ws server error");
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn ws_search_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_connection(socket, state, FeedKind::Search))
}

async fn ws_trending_today_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_connection(socket, state, FeedKind::TrendingToday))
}

// ---------------------------------------------------------------------------
// Connection handler
// ---------------------------------------------------------------------------

async fn handle_connection(socket: WebSocket, state: AppState, feed_kind: FeedKind) {
    let (mut sink, mut stream) = socket.split();

    // Spawn a ping task to keep the connection alive.
    let (close_tx, mut close_rx) = tokio::sync::oneshot::channel::<()>();
    let ping_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = &mut close_rx => break,
            }
        }
    });

    while let Some(msg_result) = stream.next().await {
        let msg = match msg_result {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!("ws read error: {e}");
                break;
            }
        };

        match msg {
            Message::Text(text) => {
                let responses = handle_nostr_message(&text, &state, &feed_kind).await;
                for r in responses {
                    if sink.send(Message::Text(r.into())).await.is_err() {
                        break;
                    }
                }
            }
            Message::Ping(data) => {
                if sink.send(Message::Pong(data)).await.is_err() {
                    break;
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    let _ = close_tx.send(());
    ping_handle.abort();
    tracing::debug!("ws connection closed");
}

// ---------------------------------------------------------------------------
// Nostr protocol handling
// ---------------------------------------------------------------------------

/// Parse and handle a Nostr protocol message. Returns response messages to send.
async fn handle_nostr_message(text: &str, state: &AppState, feed_kind: &FeedKind) -> Vec<String> {
    let parsed: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return vec![notice("invalid JSON")],
    };

    let arr = match parsed.as_array() {
        Some(a) if !a.is_empty() => a,
        _ => return vec![notice("message must be a JSON array")],
    };

    let msg_type = match arr[0].as_str() {
        Some(t) => t,
        None => return vec![notice("first element must be a string")],
    };

    match msg_type {
        "REQ" => handle_req(arr, state, feed_kind).await,
        "CLOSE" => handle_close(arr),
        "EVENT" => vec![notice("EVENT publishing is not supported on this relay")],
        _ => vec![notice(&format!("unknown message type: {msg_type}"))],
    }
}

async fn handle_req(arr: &[Value], state: &AppState, feed_kind: &FeedKind) -> Vec<String> {
    // ["REQ", subscription_id, filter, ...]
    if arr.len() < 3 {
        return vec![notice("REQ requires subscription_id and at least one filter")];
    }

    let sub_id = match arr[1].as_str() {
        Some(s) => s.to_string(),
        None => return vec![notice("subscription_id must be a string")],
    };

    match feed_kind {
        FeedKind::Search => handle_search_req(&sub_id, &arr[2..], state).await,
        FeedKind::TrendingToday => handle_trending_today_req(&sub_id, &arr[2..], state).await,
    }
}

/// Search relay: existing NIP-50 behavior (stub — returns EOSE for now).
async fn handle_search_req(sub_id: &str, filters: &[Value], _state: &AppState) -> Vec<String> {
    for filter in filters {
        if let Some(search_term) = filter.get("search").and_then(|v| v.as_str()) {
            tracing::info!(sub_id = %sub_id, search = %search_term, "NIP-50 search request");
        } else {
            tracing::debug!(sub_id = %sub_id, "REQ with non-search filter (not yet supported)");
        }
    }

    vec![eose(sub_id)]
}

/// Trending today feed: return top trending notes from the last 24h.
///
/// Any REQ on this endpoint returns the trending feed regardless of the filter contents.
/// The filter's `limit` field is respected (capped at 200, default 50).
async fn handle_trending_today_req(
    sub_id: &str,
    filters: &[Value],
    state: &AppState,
) -> Vec<String> {
    // Extract limit from the first filter (if provided)
    let limit = filters
        .first()
        .and_then(|f| f.get("limit"))
        .and_then(|v| v.as_i64())
        .unwrap_or(50)
        .clamp(1, 200);

    tracing::info!(sub_id = %sub_id, limit = limit, "trending/today feed request");

    match state.repo.trending_notes(limit, 0).await {
        Ok(notes) => {
            let mut messages = Vec::with_capacity(notes.len() + 1);

            for note in &notes {
                // Send the raw original Nostr event as stored
                let event_msg =
                    serde_json::to_string(&serde_json::json!(["EVENT", sub_id, note.event.raw.0]))
                        .expect("json serialization cannot fail");
                messages.push(event_msg);
            }

            tracing::info!(
                sub_id = %sub_id,
                events = notes.len(),
                "trending/today feed served"
            );

            messages.push(eose(sub_id));
            messages
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to fetch trending notes for feed");
            vec![
                notice(&format!("error: failed to fetch trending notes")),
                eose(sub_id),
            ]
        }
    }
}

fn handle_close(arr: &[Value]) -> Vec<String> {
    if arr.len() < 2 {
        return vec![notice("CLOSE requires subscription_id")];
    }

    let sub_id = arr[1].as_str().unwrap_or("unknown");
    tracing::debug!(sub_id = %sub_id, "subscription closed");

    vec![closed(sub_id, "")]
}

/// Format a NOTICE message.
fn notice(msg: &str) -> String {
    serde_json::to_string(&serde_json::json!(["NOTICE", msg]))
        .expect("json serialization cannot fail")
}

/// Format an EOSE message.
fn eose(sub_id: &str) -> String {
    serde_json::to_string(&serde_json::json!(["EOSE", sub_id]))
        .expect("json serialization cannot fail")
}

/// Format a CLOSED message.
fn closed(sub_id: &str, reason: &str) -> String {
    serde_json::to_string(&serde_json::json!(["CLOSED", sub_id, reason]))
        .expect("json serialization cannot fail")
}
