use std::net::SocketAddr;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, State, WebSocketUpgrade};
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
    /// Trending notes by metric and time range.
    /// metric: "reactions" | "replies" | "reposts" | "zaps"
    /// range:  "today" | "7d" | "30d" | "1y" | "all"
    Trending { metric: String, range: String },
    /// Up-and-coming users feed — returns a NIP-51 kind 30000 people list.
    UpAndComing,
}

/// Build the WebSocket relay router.
pub fn router(state: AppState) -> Router {
    Router::new()
        // Existing search relay on root path
        .route("/", any(ws_search_handler))
        // Feed endpoints: /notes/trending/{metric}/{range}
        .route(
            "/notes/trending/{metric}/{range}",
            any(ws_trending_handler),
        )
        // Up-and-coming users feed
        .route("/users/upandcoming", any(ws_upandcoming_handler))
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

async fn ws_upandcoming_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_connection(socket, state, FeedKind::UpAndComing))
}

async fn ws_trending_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Path((metric, range)): Path<(String, String)>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| {
        handle_connection(socket, state, FeedKind::Trending { metric, range })
    })
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
    if arr.len() < 3 {
        return vec![notice("REQ requires subscription_id and at least one filter")];
    }

    let sub_id = match arr[1].as_str() {
        Some(s) => s.to_string(),
        None => return vec![notice("subscription_id must be a string")],
    };

    match feed_kind {
        FeedKind::Search => handle_search_req(&sub_id, &arr[2..], state).await,
        FeedKind::Trending { metric, range } => {
            handle_trending_req(&sub_id, &arr[2..], state, metric, range).await
        }
        FeedKind::UpAndComing => {
            handle_upandcoming_req(&sub_id, &arr[2..], state).await
        }
    }
}

/// NIP-50 search relay: handles kind 0 (profiles) and kind 1 (notes) search.
///
/// Protocol:
///   Client: ["REQ", "<sub_id>", {"search": "<query>", "kinds": [0], "limit": 20}]
///   Relay:  ["EVENT", "<sub_id>", <raw_event>] ... ["EOSE", "<sub_id>"]
///
/// - Kind 0 (profiles): ranked by name match quality, follower count, engagement
/// - Kind 1 (notes): full-text search ranked by relevance + engagement + recency
/// - No kinds filter: searches both kinds
async fn handle_search_req(sub_id: &str, filters: &[Value], state: &AppState) -> Vec<String> {
    let mut messages = Vec::new();

    for filter in filters {
        let search_term = match filter.get("search").and_then(|v| v.as_str()) {
            Some(s) if !s.trim().is_empty() => s.trim(),
            _ => {
                tracing::debug!(sub_id = %sub_id, "REQ filter without search term, skipping");
                continue;
            }
        };

        let limit = filter
            .get("limit")
            .and_then(|v| v.as_i64())
            .unwrap_or(20)
            .clamp(1, 200);

        // Determine which kinds to search
        let kinds: Vec<i64> = filter
            .get("kinds")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_i64()).collect())
            .unwrap_or_default();

        let search_profiles = kinds.is_empty() || kinds.contains(&0);
        let search_notes = kinds.is_empty() || kinds.contains(&1);

        // Detect hashtag search: query starts with '#'
        let is_hashtag = search_term.starts_with('#') && search_term.len() > 1;
        let hashtag = if is_hashtag { &search_term[1..] } else { "" };

        tracing::info!(
            sub_id = %sub_id,
            search = %search_term,
            kinds = ?kinds,
            limit = limit,
            hashtag = is_hashtag,
            "NIP-50 search request"
        );

        // Hashtag search: skip profiles, use tag-based lookup for notes
        if is_hashtag && search_notes {
            match state.repo.notes_by_hashtag(hashtag, limit, 0).await {
                Ok((notes, _profiles)) => {
                    tracing::info!(
                        sub_id = %sub_id,
                        hashtag = %hashtag,
                        results = notes.len(),
                        "hashtag search completed"
                    );
                    for note in notes {
                        let event_msg = serde_json::to_string(
                            &serde_json::json!(["EVENT", sub_id, note.event.raw.0]),
                        )
                        .expect("json serialization cannot fail");
                        messages.push(event_msg);
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "hashtag search failed");
                    messages.push(notice(&format!("error: hashtag search failed: {e}")));
                }
            }
        } else {
            // Search profiles (kind 0) — skip for hashtag queries
            if search_profiles && !is_hashtag {
                match state.repo.search_profiles_as_events(search_term, limit).await {
                    Ok(events) => {
                        tracing::info!(
                            sub_id = %sub_id,
                            results = events.len(),
                            "profile search completed"
                        );
                        for event in events {
                            let event_msg = serde_json::to_string(
                                &serde_json::json!(["EVENT", sub_id, event.raw.0]),
                            )
                            .expect("json serialization cannot fail");
                            messages.push(event_msg);
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "profile search failed");
                        messages.push(notice(&format!("error: profile search failed: {e}")));
                    }
                }
            }

            // Search notes (kind 1) — full-text search
            if search_notes && !is_hashtag {
                match state.repo.search_notes_as_events(search_term, limit).await {
                    Ok(events) => {
                        tracing::info!(
                            sub_id = %sub_id,
                            results = events.len(),
                            "note search completed"
                        );
                        for event in events {
                            let event_msg = serde_json::to_string(
                                &serde_json::json!(["EVENT", sub_id, event.raw.0]),
                            )
                            .expect("json serialization cannot fail");
                            messages.push(event_msg);
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "note search failed");
                        messages.push(notice(&format!("error: note search failed: {e}")));
                    }
                }
            }
        }
    }

    messages.push(eose(sub_id));
    messages
}

// ---------------------------------------------------------------------------
// Trending feed handler
// ---------------------------------------------------------------------------

/// Validate metric string → ref_type for the DB query.
fn validate_metric(metric: &str) -> Option<&'static str> {
    match metric {
        "reactions" => Some("reaction"),
        "replies" => Some("reply"),
        "reposts" => Some("repost"),
        "zaps" => Some("zap"),
        _ => None,
    }
}

/// Validate range string → since timestamp.
fn validate_range(range: &str) -> Option<Option<i64>> {
    let now = chrono::Utc::now().timestamp();
    match range {
        "today" => Some(Some(now - 86_400)),
        "7d" => Some(Some(now - 7 * 86_400)),
        "30d" => Some(Some(now - 30 * 86_400)),
        "1y" => Some(Some(now - 365 * 86_400)),
        "all" => Some(None),
        _ => None,
    }
}

/// Trending feed: return top notes by metric and time range.
///
/// Uses the same Redis-cached path as the HTTP `/v1/notes/top` endpoint.
/// On cache hit, responses are instant. On cache miss, computes and caches.
async fn handle_trending_req(
    sub_id: &str,
    filters: &[Value],
    state: &AppState,
    metric: &str,
    range: &str,
) -> Vec<String> {
    // Validate metric
    let ref_type = match validate_metric(metric) {
        Some(rt) => rt,
        None => {
            return vec![
                notice(&format!(
                    "invalid metric: {metric}. Use: reactions, replies, reposts, zaps"
                )),
                eose(sub_id),
            ];
        }
    };

    // Validate range
    let since = match validate_range(range) {
        Some(s) => s,
        None => {
            return vec![
                notice(&format!(
                    "invalid range: {range}. Use: today, 7d, 30d, 1y, all"
                )),
                eose(sub_id),
            ];
        }
    };

    let limit = filters
        .first()
        .and_then(|f| f.get("limit"))
        .and_then(|v| v.as_i64())
        .unwrap_or(20)
        .clamp(1, 200);

    tracing::info!(
        sub_id = %sub_id,
        metric = %metric,
        range = %range,
        limit = limit,
        "feed request"
    );

    // Try Redis cache first (same cache key as the HTTP trending endpoint)
    if let Some(cached) = state.cache.get_trending(metric, range, limit, 0).await {
        if let Ok(val) = serde_json::from_str::<Value>(&cached) {
            let messages = extract_events_from_cached(&val, sub_id);
            tracing::info!(
                sub_id = %sub_id,
                metric = %metric,
                range = %range,
                events = messages.len() - 1,
                "feed served (cache hit)"
            );
            return messages;
        }
    }

    // Cache miss — compute via DB
    match state.repo.top_notes_unified(ref_type, since, limit, 0).await {
        Ok((ranked, profile_rows)) => {
            let mut messages = Vec::with_capacity(ranked.len() + 1);

            // Build the same JSON structure as the HTTP handler for caching
            let profiles: std::collections::HashMap<String, Value> = profile_rows
                .into_iter()
                .filter_map(|row| {
                    serde_json::from_str::<Value>(&row.content).ok().map(|v| {
                        let entry = serde_json::json!({
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
                .iter()
                .map(|entry| {
                    serde_json::json!({
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

            // Cache the response (same format as HTTP handler)
            let response = serde_json::json!({
                "metric": metric,
                "range": range,
                "notes": notes,
                "profiles": profiles,
            });
            if let Ok(json_str) = serde_json::to_string(&response) {
                state
                    .cache
                    .set_trending(metric, range, limit, 0, &json_str)
                    .await;
            }

            // Send raw Nostr events
            for entry in &ranked {
                let event_msg = serde_json::to_string(
                    &serde_json::json!(["EVENT", sub_id, entry.event.raw.0]),
                )
                .expect("json serialization cannot fail");
                messages.push(event_msg);
            }

            tracing::info!(
                sub_id = %sub_id,
                metric = %metric,
                range = %range,
                events = ranked.len(),
                "feed served (cache miss)"
            );

            messages.push(eose(sub_id));
            messages
        }
        Err(e) => {
            tracing::error!(
                metric = %metric,
                range = %range,
                error = %e,
                "failed to fetch trending notes for feed"
            );
            vec![
                notice("error: failed to fetch trending notes"),
                eose(sub_id),
            ]
        }
    }
}

// ---------------------------------------------------------------------------
// Up-and-coming users feed handler
// ---------------------------------------------------------------------------

/// Up-and-coming users: returns kind-0 profile events for emerging users.
///
/// Protocol:
///   Client: ["REQ", "<sub_id>", {"limit": 20}]
///   Relay:  ["EVENT", "<sub_id>", <kind-0>] ... ["EOSE", "<sub_id>"]
async fn handle_upandcoming_req(
    sub_id: &str,
    filters: &[Value],
    state: &AppState,
) -> Vec<String> {
    let limit = filters
        .first()
        .and_then(|f| f.get("limit"))
        .and_then(|v| v.as_i64())
        .unwrap_or(20)
        .clamp(1, 100);

    tracing::info!(sub_id = %sub_id, limit = limit, "up-and-coming feed request");

    // Try Redis cache first (same key the HTTP handler uses)
    let cache_key = format!("home:trending_users:{limit}:0");
    let pubkeys: Vec<String> = if let Some(cached) = state.cache.get_json(&cache_key).await {
        if let Ok(val) = serde_json::from_str::<Value>(&cached) {
            val.get("users")
                .and_then(|u| u.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|u| u.get("pubkey")?.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            vec![]
        }
    } else {
        match state.repo.trending_users(limit, 0).await {
            Ok(users) => users.into_iter().map(|u| u.pubkey).collect(),
            Err(e) => {
                tracing::error!(error = %e, "up-and-coming query failed");
                return vec![
                    notice("error: failed to fetch up-and-coming users"),
                    eose(sub_id),
                ];
            }
        }
    };

    if pubkeys.is_empty() {
        return vec![eose(sub_id)];
    }

    // Fetch latest kind-0 events for these pubkeys
    match state.repo.profile_events_for_pubkeys(&pubkeys).await {
        Ok(events) => {
            let mut messages = Vec::with_capacity(events.len() + 1);
            for event in &events {
                let msg = serde_json::to_string(
                    &serde_json::json!(["EVENT", sub_id, event.raw.0]),
                )
                .expect("json serialization cannot fail");
                messages.push(msg);
            }

            tracing::info!(
                sub_id = %sub_id,
                users = events.len(),
                "up-and-coming feed served"
            );

            messages.push(eose(sub_id));
            messages
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to fetch profile events");
            vec![
                notice("error: failed to fetch profile events"),
                eose(sub_id),
            ]
        }
    }
}

/// Extract raw Nostr events from a cached trending response JSON.
fn extract_events_from_cached(val: &Value, sub_id: &str) -> Vec<String> {
    let mut messages = Vec::new();

    if let Some(notes) = val.get("notes").and_then(|n| n.as_array()) {
        for note in notes {
            if let Some(raw) = note.get("event").and_then(|e| e.get("raw")) {
                let event_msg =
                    serde_json::to_string(&serde_json::json!(["EVENT", sub_id, raw]))
                        .expect("json serialization cannot fail");
                messages.push(event_msg);
            }
        }
    }

    messages.push(eose(sub_id));
    messages
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
