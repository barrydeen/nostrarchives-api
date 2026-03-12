use std::net::SocketAddr;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::any;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast;

use crate::api::AppState;

/// Build the WebSocket relay router.
pub fn router(state: AppState) -> Router {
    Router::new().route("/", any(ws_handler)).with_state(state)
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

async fn ws_handler(ws: WebSocketUpgrade, State(_state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(handle_connection)
}

async fn handle_connection(socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();

    // Spawn a ping task to keep the connection alive.
    let (close_tx, mut close_rx) = tokio::sync::oneshot::channel::<()>();
    let ping_handle = tokio::spawn({
        async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                tokio::select! {
                    _ = interval.tick() => {}
                    _ = &mut close_rx => break,
                }
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
                if let Some(response) = handle_nostr_message(&text) {
                    for r in response {
                        if sink.send(Message::Text(r.into())).await.is_err() {
                            break;
                        }
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

/// Parse and handle a Nostr protocol message. Returns response messages to send.
fn handle_nostr_message(text: &str) -> Option<Vec<String>> {
    let parsed: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => {
            return Some(vec![notice("invalid JSON")]);
        }
    };

    let arr = match parsed.as_array() {
        Some(a) if !a.is_empty() => a,
        _ => {
            return Some(vec![notice("message must be a JSON array")]);
        }
    };

    let msg_type = match arr[0].as_str() {
        Some(t) => t,
        None => {
            return Some(vec![notice("first element must be a string")]);
        }
    };

    match msg_type {
        "REQ" => handle_req(arr),
        "CLOSE" => handle_close(arr),
        _ => Some(vec![notice(&format!("unknown message type: {msg_type}"))]),
    }
}

fn handle_req(arr: &[serde_json::Value]) -> Option<Vec<String>> {
    // ["REQ", subscription_id, filter, ...]
    if arr.len() < 3 {
        return Some(vec![notice(
            "REQ requires subscription_id and at least one filter",
        )]);
    }

    let sub_id = match arr[1].as_str() {
        Some(s) => s.to_string(),
        None => {
            return Some(vec![notice("subscription_id must be a string")]);
        }
    };

    // Process each filter
    for filter in &arr[2..] {
        if let Some(search_term) = filter.get("search").and_then(|v| v.as_str()) {
            tracing::info!(
                sub_id = %sub_id,
                search = %search_term,
                "NIP-50 search request"
            );
        } else {
            tracing::debug!(sub_id = %sub_id, "REQ with non-search filter (not yet supported)");
        }
    }

    // For now, just return EOSE — actual search results come later
    Some(vec![eose(&sub_id)])
}

fn handle_close(arr: &[serde_json::Value]) -> Option<Vec<String>> {
    // ["CLOSE", subscription_id]
    if arr.len() < 2 {
        return Some(vec![notice("CLOSE requires subscription_id")]);
    }

    let sub_id = arr[1].as_str().unwrap_or("unknown");
    tracing::debug!(sub_id = %sub_id, "subscription closed");

    // NIP-01: relay sends CLOSED message
    Some(vec![closed(sub_id, "")])
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
