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
use crate::nip19;
use std::collections::HashMap;

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
    /// Followers feed: returns kind-0 profiles for all followers of a pubkey.
    /// Pubkey must be supplied as a single-entry `authors` filter in the REQ message.
    Followers,
    /// Ranked notes feed: a specific pubkey's root notes or replies ordered by a metric.
    /// note_type: "root" | "replies"
    /// metric: "likes" | "reposts" | "zaps" | "replies"
    /// Pubkey must be supplied as a single-entry `authors` filter in the REQ message.
    RankedNotes { note_type: String, metric: String },
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
        // Followers feed: /profiles/followers
        // Pubkey supplied via `authors` filter in the REQ message.
        .route("/profiles/followers", any(ws_followers_handler))
        // Ranked notes feeds: /profiles/{note_type}/{metric}
        // note_type: "root" or "replies"
        // metric: "likes", "reposts", "zaps", "replies"
        // Pubkey supplied via `authors` filter in the REQ message.
        .route("/profiles/{note_type}/{metric}", any(ws_ranked_notes_handler))
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

async fn ws_followers_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_connection(socket, state, FeedKind::Followers))
}

async fn ws_ranked_notes_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Path((note_type, metric)): Path<(String, String)>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| {
        handle_connection(socket, state, FeedKind::RankedNotes { note_type, metric })
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
        FeedKind::Followers => {
            handle_followers_req(&sub_id, &arr[2..], state).await
        }
        FeedKind::RankedNotes { note_type, metric } => {
            handle_ranked_notes_req(&sub_id, &arr[2..], state, &note_type, &metric).await
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
        // Check if this filter has a #t tag filter (NIP-01 tag query)
        let has_t_filter = filter
            .get("#t")
            .and_then(|v| v.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false);

        let search_term = match filter.get("search").and_then(|v| v.as_str()) {
            Some(s) if !s.trim().is_empty() => s.trim(),
            _ if has_t_filter => "", // Allow empty search when #t filter is present
            _ => {
                tracing::debug!(sub_id = %sub_id, "REQ filter without search term or #t filter, skipping");
                continue;
            }
        };

        // Entity resolution: if search term looks like a NIP-19 entity or raw hex,
        // resolve it directly and return the matching event(s) without doing full-text search.
        if nip19::looks_like_entity(search_term) {
            if let Some(event_msgs) = resolve_entity_ws(search_term, sub_id, state).await {
                messages.extend(event_msgs);
                continue;
            }
        }

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

        // NIP-01 `authors` filter: array of hex pubkeys
        let authors: Vec<String> = filter
            .get("authors")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_lowercase())
                    .collect()
            })
            .unwrap_or_default();

        // NIP-01 `#t` tag filter: array of hashtag values (OR semantics)
        let tag_filter: Vec<String> = filter
            .get("#t")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default();

        // Detect hashtag search via search field: query starts with '#'
        let is_hashtag = search_term.starts_with('#') && search_term.len() > 1;

        // Merge: #t tag filter takes priority, but also support "#tag" in search field
        let hashtags: Vec<String> = if !tag_filter.is_empty() {
            tag_filter
        } else if is_hashtag {
            // Support space-separated hashtags: "#bitcoin #nostr"
            search_term
                .split_whitespace()
                .filter_map(|w| w.strip_prefix('#'))
                .filter(|t| !t.is_empty())
                .map(|t| t.to_string())
                .collect()
        } else {
            Vec::new()
        };

        let has_hashtag_filter = !hashtags.is_empty();

        tracing::info!(
            sub_id = %sub_id,
            search = %search_term,
            kinds = ?kinds,
            limit = limit,
            hashtags = ?hashtags,
            authors = ?authors,
            "NIP-50 search request"
        );

        // Hashtag search: skip profiles, use tag-based lookup for notes
        if has_hashtag_filter && search_notes {
            match state.repo.notes_by_hashtags(&hashtags, limit, 0).await {
                Ok((notes, _profiles)) => {
                    tracing::info!(
                        sub_id = %sub_id,
                        hashtags = ?hashtags,
                        results = notes.len(),
                        "hashtag search completed"
                    );
                    for note in notes {
                        // Apply authors filter if present
                        if !authors.is_empty() && !authors.contains(&note.event.pubkey) {
                            continue;
                        }
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
            // Search profiles (kind 0) — in-memory cache for ranking, then fetch raw events.
            // Require at least 2 characters to avoid overly broad queries.
            if search_profiles && !has_hashtag_filter && search_term.len() >= 2 {
                // Phase 1: in-memory ranking (microseconds, no DB)
                let ranked = state
                    .profile_search_cache
                    .suggest_profiles(search_term, limit)
                    .await;

                if !ranked.is_empty() {
                    // Phase 2: fetch raw kind-0 events for the ranked pubkeys
                    // If authors filter is set, only include profiles matching those pubkeys
                    let pubkeys: Vec<String> = if authors.is_empty() {
                        ranked.iter().map(|p| p.pubkey.clone()).collect()
                    } else {
                        ranked.iter()
                            .filter(|p| authors.contains(&p.pubkey))
                            .map(|p| p.pubkey.clone())
                            .collect()
                    };
                    match state.repo.profile_events_for_pubkeys(&pubkeys).await {
                        Ok(events) => {
                            // Re-sort events by the in-memory ranking order
                            let order: HashMap<&str, usize> = pubkeys
                                .iter()
                                .enumerate()
                                .map(|(i, pk)| (pk.as_str(), i))
                                .collect();
                            let mut sorted = events;
                            sorted.sort_by_key(|e| order.get(e.pubkey.as_str()).copied().unwrap_or(usize::MAX));

                            tracing::info!(
                                sub_id = %sub_id,
                                results = sorted.len(),
                                "profile search completed (in-memory)"
                            );
                            for event in sorted {
                                let event_msg = serde_json::to_string(
                                    &serde_json::json!(["EVENT", sub_id, event.raw.0]),
                                )
                                .expect("json serialization cannot fail");
                                messages.push(event_msg);
                            }
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "profile event fetch failed");
                            messages.push(notice(&format!("error: profile search failed: {e}")));
                        }
                    }
                } else {
                    tracing::info!(
                        sub_id = %sub_id,
                        "profile search: no in-memory matches"
                    );
                }
            }

            // Search notes (kind 1) — full-text search (skip when hashtag filter or empty search)
            if search_notes && !has_hashtag_filter && !search_term.is_empty() {
                let authors_ref: Vec<&str> = authors.iter().map(|s| s.as_str()).collect();
                match state.repo.search_notes_as_events(search_term, limit, &authors_ref).await {
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
// Entity resolution helper for NIP-50 search
// ---------------------------------------------------------------------------

/// Try to resolve a NIP-19 entity or 64-char hex as a Nostr event/profile.
/// Returns a list of EVENT message strings to send, or `None` if unresolvable.
async fn resolve_entity_ws(input: &str, sub_id: &str, state: &AppState) -> Option<Vec<String>> {
    if let Some(entity) = nip19::decode(input) {
        match &entity {
            nip19::NostrEntity::Event { id, relays, .. } => {
                // On-demand fetch if not in DB
                if state.repo.get_event_by_id(id).await.ok()?.is_none() {
                    let _ = state.fetcher.fetch_event_by_id(id, relays).await;
                }
                if let Some(event) = state.repo.get_event_by_id(id).await.ok()? {
                    let msg = serde_json::to_string(
                        &serde_json::json!(["EVENT", sub_id, event.raw.0]),
                    )
                    .expect("json serialization cannot fail");
                    return Some(vec![msg]);
                }
                return Some(vec![]); // resolved as entity type but not found in DB
            }
            nip19::NostrEntity::Profile { pubkey, relays } => {
                // On-demand fetch if not in DB
                let profile_rows = state
                    .repo
                    .latest_profile_metadata(&[pubkey.clone()])
                    .await
                    .ok()?;
                if profile_rows.is_empty() {
                    let _ = state.fetcher.fetch_profile_metadata(pubkey, relays).await;
                }
                let events = state
                    .repo
                    .profile_events_for_pubkeys(&[pubkey.clone()])
                    .await
                    .ok()?;
                let msgs: Vec<String> = events
                    .iter()
                    .map(|e| {
                        serde_json::to_string(&serde_json::json!(["EVENT", sub_id, e.raw.0]))
                            .expect("json serialization cannot fail")
                    })
                    .collect();
                return Some(msgs);
            }
        }
    }

    // Raw 64-char hex: resolve via DB (no on-demand fetch to avoid abuse)
    if nip19::is_hex64(input) {
        if let Ok(Some((entity_type, id))) = state.repo.resolve_hex(input).await {
            if entity_type == "event" {
                if let Ok(Some(event)) = state.repo.get_event_by_id(&id).await {
                    let msg = serde_json::to_string(
                        &serde_json::json!(["EVENT", sub_id, event.raw.0]),
                    )
                    .expect("json serialization cannot fail");
                    return Some(vec![msg]);
                }
            } else {
                // Profile hex
                if let Ok(events) = state.repo.profile_events_for_pubkeys(&[id]).await {
                    let msgs: Vec<String> = events
                        .iter()
                        .map(|e| {
                            serde_json::to_string(
                                &serde_json::json!(["EVENT", sub_id, e.raw.0]),
                            )
                            .expect("json serialization cannot fail")
                        })
                        .collect();
                    return Some(msgs);
                }
            }
        }
    }

    None
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
    let filter = filters.first().unwrap_or(&Value::Null);

    // Respect the kinds filter — this feed only serves kind 0 (profiles).
    // If client requests specific kinds that don't include 0, return empty.
    let kinds: Vec<i64> = filter
        .get("kinds")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_i64()).collect())
        .unwrap_or_default();

    if !kinds.is_empty() && !kinds.contains(&0) {
        tracing::debug!(sub_id = %sub_id, ?kinds, "up-and-coming: kinds filter excludes kind 0, returning empty");
        return vec![eose(sub_id)];
    }

    let limit = filter
        .get("limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(20)
        .clamp(1, 100);

    tracing::info!(sub_id = %sub_id, limit = limit, "up-and-coming feed request");

    // Cache the full list of raw kind-0 event JSON so repeat requests are instant.
    let cache_key = format!("ws:upandcoming:{limit}");

    // Try cache first
    if let Some(cached) = state.cache.get_json(&cache_key).await {
        if let Ok(raw_events) = serde_json::from_str::<Vec<Value>>(&cached) {
            let mut messages = Vec::with_capacity(raw_events.len() + 1);
            for raw in &raw_events {
                let msg = serde_json::to_string(&serde_json::json!(["EVENT", sub_id, raw]))
                    .expect("json serialization cannot fail");
                messages.push(msg);
            }
            tracing::info!(sub_id = %sub_id, users = raw_events.len(), "up-and-coming feed served (cache hit)");
            messages.push(eose(sub_id));
            return messages;
        }
    }

    // Cache miss — get trending user pubkeys
    let pubkeys: Vec<String> = match state.repo.trending_users(limit, 0).await {
        Ok(users) => users.into_iter().map(|u| u.pubkey).collect(),
        Err(e) => {
            tracing::error!(error = %e, "up-and-coming query failed");
            return vec![
                notice("error: failed to fetch up-and-coming users"),
                eose(sub_id),
            ];
        }
    };

    if pubkeys.is_empty() {
        return vec![eose(sub_id)];
    }

    // Fetch latest kind-0 events
    match state.repo.profile_events_for_pubkeys(&pubkeys).await {
        Ok(events) => {
            let raw_events: Vec<&Value> = events.iter().map(|e| &e.raw.0).collect();
            let mut messages = Vec::with_capacity(events.len() + 1);
            for raw in &raw_events {
                let msg = serde_json::to_string(&serde_json::json!(["EVENT", sub_id, raw]))
                    .expect("json serialization cannot fail");
                messages.push(msg);
            }

            // Cache for 24 hours — this data changes slowly and initial load is expensive
            if let Ok(json_str) = serde_json::to_string(&raw_events) {
                state.cache.set_json(&cache_key, &json_str, 86_400).await;
            }

            tracing::info!(sub_id = %sub_id, users = events.len(), "up-and-coming feed served (cache miss)");
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

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract exactly one hex pubkey from the `authors` field of the first filter.
/// Returns `Err` with a human-readable notice string on any violation.
fn extract_single_author(filters: &[Value]) -> Result<String, String> {
    let filter = filters.first().unwrap_or(&Value::Null);
    let authors = filter
        .get("authors")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "filter must include an `authors` array".to_string())?;

    if authors.len() != 1 {
        return Err(format!(
            "`authors` must contain exactly one pubkey, got {}",
            authors.len()
        ));
    }

    let pubkey = authors[0]
        .as_str()
        .ok_or_else(|| "pubkey must be a string".to_string())?;

    if pubkey.len() != 64 || !pubkey.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("invalid pubkey: {pubkey}"));
    }

    Ok(pubkey.to_string())
}

// ---------------------------------------------------------------------------
// Followers feed handler
// ---------------------------------------------------------------------------

/// Followers feed: returns kind-0 profile events for ALL followers of a pubkey.
///
/// Protocol:
///   Client: ["REQ", "<sub_id>", {"authors": ["<hex_pubkey>"]}]
///   Relay:  ["EVENT", "<sub_id>", <kind-0>] ... ["EOSE", "<sub_id>"]
async fn handle_followers_req(
    sub_id: &str,
    filters: &[Value],
    state: &AppState,
) -> Vec<String> {
    // Extract exactly one author from the filter
    let pubkey = match extract_single_author(filters) {
        Ok(pk) => pk,
        Err(msg) => return vec![notice(&msg), eose(sub_id)],
    };
    let pubkey = pubkey.as_str();

    tracing::info!(sub_id = %sub_id, pubkey = %pubkey, "followers feed request");

    // Cache key for this follower list
    let cache_key = format!("ws:followers:{pubkey}");

    // Try cache first
    if let Some(cached) = state.cache.get_json(&cache_key).await {
        if let Ok(raw_events) = serde_json::from_str::<Vec<Value>>(&cached) {
            let mut messages = Vec::with_capacity(raw_events.len() + 1);
            for raw in &raw_events {
                let msg = serde_json::to_string(&serde_json::json!(["EVENT", sub_id, raw]))
                    .expect("json serialization cannot fail");
                messages.push(msg);
            }
            tracing::info!(sub_id = %sub_id, pubkey = %pubkey, profiles = raw_events.len(), "followers feed served (cache hit)");
            messages.push(eose(sub_id));
            return messages;
        }
    }

    // Cache miss — get all follower pubkeys
    let follower_pubkeys = match state.repo.all_follower_pubkeys(pubkey).await {
        Ok(pks) => pks,
        Err(e) => {
            tracing::error!(error = %e, "followers query failed");
            return vec![
                notice("error: failed to fetch followers"),
                eose(sub_id),
            ];
        }
    };

    if follower_pubkeys.is_empty() {
        return vec![eose(sub_id)];
    }

    tracing::info!(sub_id = %sub_id, pubkey = %pubkey, followers = follower_pubkeys.len(), "fetching kind-0 profiles for followers");

    // Fetch kind-0 events in batches to avoid overly large queries
    let mut all_events = Vec::new();
    for chunk in follower_pubkeys.chunks(500) {
        match state.repo.profile_events_for_pubkeys(&chunk.to_vec()).await {
            Ok(events) => all_events.extend(events),
            Err(e) => {
                tracing::error!(error = %e, "profile event fetch failed for follower batch");
            }
        }
    }

    let raw_events: Vec<&Value> = all_events.iter().map(|e| &e.raw.0).collect();
    let mut messages = Vec::with_capacity(all_events.len() + 1);
    for raw in &raw_events {
        let msg = serde_json::to_string(&serde_json::json!(["EVENT", sub_id, raw]))
            .expect("json serialization cannot fail");
        messages.push(msg);
    }

    // Cache for 10 minutes
    if let Ok(json_str) = serde_json::to_string(&raw_events) {
        state.cache.set_json(&cache_key, &json_str, 600).await;
    }

    tracing::info!(sub_id = %sub_id, pubkey = %pubkey, profiles = all_events.len(), "followers feed served (cache miss)");
    messages.push(eose(sub_id));
    messages
}

// ---------------------------------------------------------------------------
// Ranked notes feed handler
// ---------------------------------------------------------------------------

/// Validate note_type: "root" or "replies".
fn validate_note_type(note_type: &str) -> bool {
    matches!(note_type, "root" | "replies")
}

/// Validate ranking metric: "likes", "reposts", "zaps", "replies".
fn validate_ranking_metric(metric: &str) -> bool {
    matches!(metric, "likes" | "reposts" | "zaps" | "replies")
}

/// Ranked notes feed: a pubkey's root notes or replies ordered by a specific metric.
///
/// Routes:
///   /profiles/root/likes      — pubkey's root notes ordered by reaction_count
///   /profiles/root/reposts    — pubkey's root notes ordered by repost_count
///   /profiles/root/zaps       — pubkey's root notes ordered by zap_amount_msats
///   /profiles/root/replies    — pubkey's root notes ordered by reply_count
///   /profiles/replies/likes   — pubkey's replies ordered by reaction_count
///   /profiles/replies/reposts — pubkey's replies ordered by repost_count
///   /profiles/replies/zaps    — pubkey's replies ordered by zap_amount_msats
///   /profiles/replies/replies — pubkey's replies ordered by reply_count
///
/// Protocol:
///   Client: ["REQ", "<sub_id>", {"authors": ["<hex_pubkey>"], "limit": 50}]
///   Relay:  ["EVENT", "<sub_id>", <kind-1>] ... ["EOSE", "<sub_id>"]
async fn handle_ranked_notes_req(
    sub_id: &str,
    filters: &[Value],
    state: &AppState,
    note_type: &str,
    metric: &str,
) -> Vec<String> {
    // Extract exactly one author from the filter
    let pubkey_owned = match extract_single_author(filters) {
        Ok(pk) => pk,
        Err(msg) => return vec![notice(&msg), eose(sub_id)],
    };
    let pubkey = pubkey_owned.as_str();

    // Validate note_type
    if !validate_note_type(note_type) {
        return vec![
            notice(&format!(
                "invalid note_type: {note_type}. Use: root, replies"
            )),
            eose(sub_id),
        ];
    }

    // Validate metric
    if !validate_ranking_metric(metric) {
        return vec![
            notice(&format!(
                "invalid metric: {metric}. Use: likes, reposts, zaps, replies"
            )),
            eose(sub_id),
        ];
    }

    let filter = filters.first().unwrap_or(&Value::Null);
    let limit = filter
        .get("limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(50)
        .clamp(1, 500);

    let is_reply = note_type == "replies";

    tracing::info!(
        sub_id = %sub_id,
        pubkey = %pubkey,
        note_type = %note_type,
        metric = %metric,
        limit = limit,
        "ranked notes feed request"
    );

    // Cache key (per pubkey)
    let cache_key = format!("ws:ranked:{pubkey}:{note_type}:{metric}:{limit}");

    // Try cache first
    if let Some(cached) = state.cache.get_json(&cache_key).await {
        if let Ok(raw_events) = serde_json::from_str::<Vec<Value>>(&cached) {
            let mut messages = Vec::with_capacity(raw_events.len() + 1);
            for raw in &raw_events {
                let msg = serde_json::to_string(&serde_json::json!(["EVENT", sub_id, raw]))
                    .expect("json serialization cannot fail");
                messages.push(msg);
            }
            tracing::info!(
                sub_id = %sub_id,
                note_type = %note_type,
                metric = %metric,
                events = raw_events.len(),
                "ranked notes feed served (cache hit)"
            );
            messages.push(eose(sub_id));
            return messages;
        }
    }

    // Cache miss — query DB
    match state.repo.ranked_notes_by_pubkey(pubkey, is_reply, metric, limit, 0).await {
        Ok((ranked, _profiles)) => {
            let raw_events: Vec<&Value> = ranked.iter().map(|e| &e.event.raw.0).collect();
            let mut messages = Vec::with_capacity(ranked.len() + 1);
            for raw in &raw_events {
                let msg = serde_json::to_string(&serde_json::json!(["EVENT", sub_id, raw]))
                    .expect("json serialization cannot fail");
                messages.push(msg);
            }

            // Cache for 5 minutes
            if let Ok(json_str) = serde_json::to_string(&raw_events) {
                state.cache.set_json(&cache_key, &json_str, 300).await;
            }

            tracing::info!(
                sub_id = %sub_id,
                note_type = %note_type,
                metric = %metric,
                events = ranked.len(),
                "ranked notes feed served (cache miss)"
            );
            messages.push(eose(sub_id));
            messages
        }
        Err(e) => {
            tracing::error!(
                note_type = %note_type,
                metric = %metric,
                error = %e,
                "failed to fetch ranked notes"
            );
            vec![
                notice("error: failed to fetch ranked notes"),
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
