//! Indexer relay: a restricted Nostr relay endpoint that serves only profile
//! metadata (kind 0), follow lists (kind 3), and relay lists (kind 10002).
//!
//! Endpoint: `wss://indexer.nostrarchives.com`
//!
//! Constraints:
//! - Only REQ and CLOSE messages are accepted (no EVENT publishing)
//! - Filters MUST include an `authors` array (1–500 hex pubkeys)
//! - Filters MUST request only kinds 0, 3, and/or 10002
//! - Maximum 500 results per request
//! - Rate limited to 30 requests per minute per IP

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{ConnectInfo, State, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::any;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::broadcast;
use tokio::sync::Mutex;

use crate::db::models::StoredEvent;
use crate::db::repository::EventRepository;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Allowed kinds on this relay.
const ALLOWED_KINDS: &[i64] = &[0, 3, 10002];

/// Maximum number of authors per filter.
const MAX_AUTHORS: usize = 500;

/// Maximum total results per REQ (across all filters).
const MAX_RESULTS: usize = 500;

/// Rate limit: requests per window.
const RATE_LIMIT_MAX: u32 = 30;

/// Rate limit: window duration.
const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct IndexerState {
    pub repo: EventRepository,
    rate_limiter: Arc<Mutex<HashMap<IpAddr, Vec<Instant>>>>,
}

impl IndexerState {
    pub fn new(repo: EventRepository) -> Self {
        Self {
            repo,
            rate_limiter: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Check rate limit for an IP. Returns Ok(remaining) or Err(retry_after_secs).
    async fn check_rate_limit(&self, ip: IpAddr) -> Result<u32, u64> {
        let now = Instant::now();
        let mut buckets = self.rate_limiter.lock().await;

        // Periodic cleanup
        if buckets.len() > 10_000 {
            buckets.retain(|_, ts| {
                ts.last()
                    .map_or(false, |t| now.duration_since(*t) < RATE_LIMIT_WINDOW)
            });
        }

        let timestamps = buckets.entry(ip).or_default();
        let cutoff = now - RATE_LIMIT_WINDOW;
        timestamps.retain(|t| *t > cutoff);

        if timestamps.len() as u32 >= RATE_LIMIT_MAX {
            let oldest = timestamps.first().unwrap();
            let retry_after = RATE_LIMIT_WINDOW - now.duration_since(*oldest);
            Err(retry_after.as_secs() + 1)
        } else {
            timestamps.push(now);
            Ok(RATE_LIMIT_MAX - timestamps.len() as u32)
        }
    }
}

// ---------------------------------------------------------------------------
// Router + Server
// ---------------------------------------------------------------------------

pub fn router(state: IndexerState) -> Router {
    Router::new()
        .route("/", any(ws_handler))
        .with_state(state)
}

pub async fn serve(
    state: IndexerState,
    addr: SocketAddr,
    mut shutdown_rx: broadcast::Receiver<()>,
) {
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind indexer listener");

    tracing::info!(addr = %addr, "indexer relay listening");

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let _ = shutdown_rx.recv().await;
    })
    .await
    .expect("indexer server error");
}

// ---------------------------------------------------------------------------
// WebSocket handler
// ---------------------------------------------------------------------------

async fn ws_handler(
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<IndexerState>,
) -> impl IntoResponse {
    let ip = addr.ip();
    ws.on_upgrade(move |socket| handle_connection(socket, state, ip))
}

async fn handle_connection(socket: WebSocket, state: IndexerState, client_ip: IpAddr) {
    let (mut sink, mut stream) = socket.split();

    // Send relay info on connect
    let info = serde_json::json!({
        "name": "nostrarchives-indexer",
        "description": "Restricted relay serving profile metadata (kind 0), follow lists (kind 3), and relay lists (kind 10002)",
        "supported_nips": [1, 11],
        "software": "nostrarchives-indexer",
        "limitation": {
            "max_filters": 10,
            "max_authors": MAX_AUTHORS,
            "max_results": MAX_RESULTS,
            "rate_limit_per_minute": RATE_LIMIT_MAX,
            "allowed_kinds": ALLOWED_KINDS,
            "auth_required": false,
            "payment_required": false,
        }
    });

    // Keepalive ping
    let (close_tx, mut close_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
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
                tracing::debug!("indexer ws read error: {e}");
                break;
            }
        };

        match msg {
            Message::Text(text) => {
                let responses = handle_message(&text, &state, client_ip).await;
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
    tracing::debug!("indexer ws connection closed");
}

// ---------------------------------------------------------------------------
// Message handling
// ---------------------------------------------------------------------------

async fn handle_message(text: &str, state: &IndexerState, ip: IpAddr) -> Vec<String> {
    let parsed: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => {
            return vec![notice(
                "error: invalid JSON. Expected a JSON array per NIP-01.",
            )]
        }
    };

    let arr = match parsed.as_array() {
        Some(a) if !a.is_empty() => a,
        _ => {
            return vec![notice(
                "error: message must be a non-empty JSON array. Format: [\"REQ\", <sub_id>, <filter>, ...] or [\"CLOSE\", <sub_id>]",
            )]
        }
    };

    let msg_type = match arr[0].as_str() {
        Some(t) => t,
        None => {
            return vec![notice(
                "error: first element must be a string message type (\"REQ\" or \"CLOSE\").",
            )]
        }
    };

    match msg_type {
        "REQ" => {
            // Rate limit check on REQ only
            match state.check_rate_limit(ip).await {
                Ok(_remaining) => {}
                Err(retry_after) => {
                    return vec![notice(&format!(
                        "error: rate limit exceeded ({RATE_LIMIT_MAX} requests per minute). Retry after {retry_after} seconds."
                    ))];
                }
            }
            handle_req(arr, state).await
        }
        "CLOSE" => handle_close(arr),
        "EVENT" => vec![notice(
            "error: EVENT publishing is not supported. This is a read-only indexer relay.",
        )],
        other => vec![notice(&format!(
            "error: unknown message type \"{other}\". Supported: REQ, CLOSE.",
        ))],
    }
}

// ---------------------------------------------------------------------------
// REQ handler
// ---------------------------------------------------------------------------

async fn handle_req(arr: &[Value], state: &IndexerState) -> Vec<String> {
    // Validate structure: ["REQ", <sub_id>, <filter>, ...]
    if arr.len() < 3 {
        return vec![notice(
            "error: REQ requires at least 3 elements: [\"REQ\", \"<subscription_id>\", {<filter>}]. See NIP-01.",
        )];
    }

    let sub_id = match arr[1].as_str() {
        Some(s) if !s.is_empty() => s.to_string(),
        Some(_) => {
            return vec![notice(
                "error: subscription_id cannot be empty.",
            )]
        }
        None => {
            return vec![notice(
                "error: subscription_id (second element) must be a string.",
            )]
        }
    };

    let filters = &arr[2..];
    if filters.len() > 10 {
        return vec![
            closed(
                &sub_id,
                "error: too many filters. Maximum 10 filters per REQ.",
            ),
        ];
    }

    // Validate all filters before executing any queries
    for (i, filter) in filters.iter().enumerate() {
        if !filter.is_object() {
            return vec![closed(
                &sub_id,
                &format!("error: filter #{} must be a JSON object.", i + 1),
            )];
        }

        // Validate kinds
        if let Err(msg) = validate_kinds(filter, i) {
            return vec![closed(&sub_id, &msg)];
        }

        // Validate authors
        if let Err(msg) = validate_authors(filter, i) {
            return vec![closed(&sub_id, &msg)];
        }
    }

    // Execute queries
    let mut total_sent = 0usize;
    let mut messages = Vec::new();

    for filter in filters {
        if total_sent >= MAX_RESULTS {
            break;
        }

        let remaining = MAX_RESULTS - total_sent;
        match execute_filter(filter, state, remaining).await {
            Ok(events) => {
                total_sent += events.len();
                for event in events {
                    let event_msg = serde_json::to_string(
                        &serde_json::json!(["EVENT", &sub_id, event.raw.0]),
                    )
                    .expect("json serialization cannot fail");
                    messages.push(event_msg);
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "indexer query failed");
                messages.push(notice(&format!("error: internal query failed: {e}")));
            }
        }
    }

    messages.push(eose(&sub_id));
    messages
}

// ---------------------------------------------------------------------------
// Filter validation
// ---------------------------------------------------------------------------

/// Validate that the kinds field, if present, only contains allowed kinds.
/// If kinds is absent, default to all allowed kinds.
fn validate_kinds(filter: &Value, filter_idx: usize) -> Result<(), String> {
    let kinds = match filter.get("kinds") {
        Some(v) => v,
        None => return Ok(()), // No kinds = all allowed kinds (we filter server-side)
    };

    let kinds_arr = match kinds.as_array() {
        Some(a) => a,
        None => {
            return Err(format!(
                "error: filter #{}: \"kinds\" must be an array of integers.",
                filter_idx + 1
            ))
        }
    };

    if kinds_arr.is_empty() {
        return Err(format!(
            "error: filter #{}: \"kinds\" array cannot be empty. Allowed kinds: 0 (profile metadata), 3 (follow list), 10002 (relay list).",
            filter_idx + 1
        ));
    }

    let mut seen = HashSet::new();
    for k in kinds_arr {
        let kind = match k.as_i64() {
            Some(n) => n,
            None => {
                return Err(format!(
                    "error: filter #{}: kinds must be integers. Got: {}.",
                    filter_idx + 1,
                    k
                ))
            }
        };

        if !ALLOWED_KINDS.contains(&kind) {
            let kind_name = match kind {
                1 => "short text note",
                6 | 16 => "repost",
                7 => "reaction",
                9735 => "zap receipt",
                _ => "unknown",
            };
            return Err(format!(
                "error: filter #{}: kind {kind} ({kind_name}) is not available on this relay. Allowed kinds: 0 (profile metadata), 3 (follow list), 10002 (relay list).",
                filter_idx + 1,
            ));
        }

        seen.insert(kind);
    }

    Ok(())
}

/// Validate that the authors field is present and within bounds.
fn validate_authors(filter: &Value, filter_idx: usize) -> Result<(), String> {
    let authors = match filter.get("authors") {
        Some(v) => v,
        None => {
            return Err(format!(
                "error: filter #{}: \"authors\" field is required. Provide an array of 1-{MAX_AUTHORS} hex pubkeys.",
                filter_idx + 1
            ))
        }
    };

    let authors_arr = match authors.as_array() {
        Some(a) => a,
        None => {
            return Err(format!(
                "error: filter #{}: \"authors\" must be an array of hex pubkey strings.",
                filter_idx + 1
            ))
        }
    };

    if authors_arr.is_empty() {
        return Err(format!(
            "error: filter #{}: \"authors\" array cannot be empty. Provide 1-{MAX_AUTHORS} hex pubkeys.",
            filter_idx + 1
        ));
    }

    if authors_arr.len() > MAX_AUTHORS {
        return Err(format!(
            "error: filter #{}: too many authors ({}). Maximum is {MAX_AUTHORS} per filter.",
            filter_idx + 1,
            authors_arr.len()
        ));
    }

    // Validate each pubkey is a valid hex string
    for (j, author) in authors_arr.iter().enumerate() {
        match author.as_str() {
            Some(s) => {
                if s.len() != 64 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Err(format!(
                        "error: filter #{}: authors[{j}] is not a valid 64-character hex pubkey: \"{s}\".",
                        filter_idx + 1
                    ));
                }
            }
            None => {
                return Err(format!(
                    "error: filter #{}: authors[{j}] must be a string (hex pubkey).",
                    filter_idx + 1
                ))
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Query execution
// ---------------------------------------------------------------------------

async fn execute_filter(
    filter: &Value,
    state: &IndexerState,
    max_results: usize,
) -> Result<Vec<StoredEvent>, crate::error::AppError> {
    // Extract authors
    let authors: Vec<String> = filter
        .get("authors")
        .and_then(|v| v.as_array())
        .unwrap() // Already validated
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();

    // Extract kinds (default to all allowed)
    let kinds: Vec<i32> = filter
        .get("kinds")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_i64().map(|n| n as i32)).collect())
        .unwrap_or_else(|| ALLOWED_KINDS.iter().map(|&k| k as i32).collect());

    // Extract optional since/until
    let since = filter.get("since").and_then(|v| v.as_i64());
    let until = filter.get("until").and_then(|v| v.as_i64());

    // Limit: respect both filter limit and remaining budget
    let filter_limit = filter
        .get("limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(MAX_RESULTS as i64)
        .clamp(1, MAX_RESULTS as i64) as usize;
    let limit = filter_limit.min(max_results);

    state
        .repo
        .events_by_kinds_and_authors(&kinds, &authors, since, until, limit as i64)
        .await
}

// ---------------------------------------------------------------------------
// CLOSE handler
// ---------------------------------------------------------------------------

fn handle_close(arr: &[Value]) -> Vec<String> {
    if arr.len() < 2 {
        return vec![notice(
            "error: CLOSE requires a subscription_id: [\"CLOSE\", \"<subscription_id>\"].",
        )];
    }

    let sub_id = arr[1].as_str().unwrap_or("unknown");
    tracing::debug!(sub_id = %sub_id, "indexer subscription closed");
    vec![closed(sub_id, "")]
}

// ---------------------------------------------------------------------------
// Protocol helpers
// ---------------------------------------------------------------------------

fn notice(msg: &str) -> String {
    serde_json::to_string(&serde_json::json!(["NOTICE", msg]))
        .expect("json serialization cannot fail")
}

fn eose(sub_id: &str) -> String {
    serde_json::to_string(&serde_json::json!(["EOSE", sub_id]))
        .expect("json serialization cannot fail")
}

fn closed(sub_id: &str, reason: &str) -> String {
    serde_json::to_string(&serde_json::json!(["CLOSED", sub_id, reason]))
        .expect("json serialization cannot fail")
}
