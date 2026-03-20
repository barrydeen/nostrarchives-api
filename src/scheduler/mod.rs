//! Scheduler relay: accepts future-dated Nostr events and publishes them at the scheduled time.
//!
//! WebSocket endpoint at `wss://scheduler.nostrarchives.com`
//!
//! ## Authentication (NIP-42)
//!
//! On connect the server immediately sends `["AUTH", <challenge>]`. Clients must respond
//! with a signed kind-22242 event containing a `["challenge", "<value>"]` tag before any
//! other message will be accepted.
//!
//! ## Supported messages (all require prior AUTH)
//!
//! - `["AUTH", <kind-22242-event>]`   — Authenticate (must be first)
//! - `["EVENT", <event>]`             — Submit a future-dated event for scheduling.
//!                                      The event pubkey must match the authenticated pubkey.
//! - `["EVENT", <kind-5-event>]`      — Cancel pending scheduled events (NIP-09).
//!                                      References target event IDs via `e` tags.
//!                                      Only the owning pubkey may cancel their own events.
//! - `["REQ", <sub_id>, <filter>]`    — Query your own scheduled events.
//!                                      Always returns only events belonging to the
//!                                      authenticated pubkey — the filter's `authors` field
//!                                      is ignored to prevent data leakage.
//! - `["CLOSE", <sub_id>]`            — Close a subscription.
//!
//! ## Publishing
//!
//! The scheduler only accepts events with `created_at` in the future.
//! A background task checks every 60 seconds for events due to be published,
//! looks up the author's NIP-65 write relays (falling back to top 20 relays),
//! and publishes them.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::any;
use axum::Router;
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite;
use uuid::Uuid;

use crate::crawler::relay_router::RelayRouter;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum number of pending scheduled events per pubkey.
const MAX_PENDING_PER_PUBKEY: i64 = 100;

/// How far into the future we allow scheduling (90 days).
const MAX_FUTURE_SECS: i64 = 90 * 86_400;

/// Minimum seconds into the future (must be at least 60s ahead).
const MIN_FUTURE_SECS: i64 = 60;

/// Number of fallback relays when user has no NIP-65 relay list.
const FALLBACK_RELAY_COUNT: i64 = 20;

/// How many relays to publish to concurrently.
const PUBLISH_CONCURRENCY: usize = 10;

/// Timeout for connecting + sending to a single relay.
const RELAY_SEND_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct SchedulerState {
    pub pool: PgPool,
    pub relay_router: RelayRouter,
    /// Cached top relays, refreshed periodically.
    pub top_relays: Arc<tokio::sync::RwLock<Vec<String>>>,
}

// ---------------------------------------------------------------------------
// Router + Server
// ---------------------------------------------------------------------------

pub fn router(state: SchedulerState) -> Router {
    Router::new()
        .route("/", any(ws_handler))
        .with_state(state)
}

pub async fn serve(
    state: SchedulerState,
    addr: SocketAddr,
    mut shutdown_rx: broadcast::Receiver<()>,
) {
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind scheduler ws listener");

    tracing::info!(addr = %addr, "scheduler relay listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.recv().await;
        })
        .await
        .expect("scheduler ws server error");
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<SchedulerState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_connection(socket, state))
}

// ---------------------------------------------------------------------------
// Per-connection authentication state (NIP-42)
// ---------------------------------------------------------------------------

struct ConnectionState {
    /// Random challenge sent to the client on connect.
    auth_challenge: String,
    /// Pubkey of the authenticated client, set after successful AUTH.
    auth_pubkey: Option<String>,
}

// ---------------------------------------------------------------------------
// Connection handler
// ---------------------------------------------------------------------------

async fn handle_connection(socket: WebSocket, state: SchedulerState) {
    let (mut sink, mut stream) = socket.split();

    // NIP-42: send AUTH challenge immediately on connect.
    let challenge = Uuid::new_v4().to_string();
    let auth_msg = serde_json::to_string(&serde_json::json!(["AUTH", challenge]))
        .expect("json serialization cannot fail");
    if sink.send(Message::Text(auth_msg.into())).await.is_err() {
        return;
    }

    let mut conn = ConnectionState {
        auth_challenge: challenge,
        auth_pubkey: None,
    };

    while let Some(msg_result) = stream.next().await {
        let msg = match msg_result {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!("scheduler ws read error: {e}");
                break;
            }
        };

        match msg {
            Message::Text(text) => {
                let responses = handle_nostr_message(&text, &state, &mut conn).await;
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

    tracing::debug!("scheduler ws connection closed");
}

// ---------------------------------------------------------------------------
// Nostr protocol handling
// ---------------------------------------------------------------------------

async fn handle_nostr_message(
    text: &str,
    state: &SchedulerState,
    conn: &mut ConnectionState,
) -> Vec<String> {
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
        "AUTH" => handle_auth(arr, conn),
        "EVENT" => handle_event(arr, state, conn).await,
        "REQ" => handle_req(arr, state, conn).await,
        "CLOSE" => handle_close(arr),
        _ => vec![notice(&format!("unknown message type: {msg_type}"))],
    }
}

// ---------------------------------------------------------------------------
// AUTH handler — NIP-42 client authentication
// ---------------------------------------------------------------------------

fn handle_auth(arr: &[Value], conn: &mut ConnectionState) -> Vec<String> {
    if arr.len() < 2 {
        return vec![notice("AUTH requires an event object")];
    }

    let event = &arr[1];

    let id = match event.get("id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return vec![ok_msg("", false, "missing event id")],
    };

    let pubkey = match event.get("pubkey").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return vec![ok_msg(id, false, "missing pubkey")],
    };

    let created_at = match event.get("created_at").and_then(|v| v.as_i64()) {
        Some(ts) => ts,
        None => return vec![ok_msg(id, false, "missing or invalid created_at")],
    };

    let kind = match event.get("kind").and_then(|v| v.as_i64()) {
        Some(k) => k,
        None => return vec![ok_msg(id, false, "missing or invalid kind")],
    };

    if kind != 22242 {
        return vec![ok_msg(id, false, "auth event must be kind 22242")];
    }

    let content = event.get("content").and_then(|v| v.as_str()).unwrap_or("");

    let sig = match event.get("sig").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return vec![ok_msg(id, false, "missing signature")],
    };

    let tags = event
        .get("tags")
        .cloned()
        .unwrap_or_else(|| Value::Array(vec![]));

    if let Err(e) = verify_event_id(id, pubkey, created_at, kind, &tags, content) {
        return vec![ok_msg(id, false, &e)];
    }

    if let Err(e) = verify_signature(id, pubkey, sig) {
        return vec![ok_msg(id, false, &e)];
    }

    // created_at must be within 10 minutes of now
    let now = Utc::now().timestamp();
    if (now - created_at).abs() > 600 {
        return vec![ok_msg(
            id,
            false,
            "invalid: auth event created_at too far from current time",
        )];
    }

    // Must have a challenge tag matching our challenge
    let challenge_val = tags
        .as_array()
        .and_then(|arr| {
            arr.iter().find(|tag| {
                tag.as_array()
                    .and_then(|t| t.first())
                    .and_then(|v| v.as_str())
                    == Some("challenge")
            })
        })
        .and_then(|tag| tag.as_array()?.get(1)?.as_str());

    match challenge_val {
        Some(c) if c == conn.auth_challenge => {}
        Some(_) => return vec![ok_msg(id, false, "invalid: challenge mismatch")],
        None => return vec![ok_msg(id, false, "invalid: missing challenge tag")],
    }

    conn.auth_pubkey = Some(pubkey.to_string());
    tracing::info!(pubkey = %pubkey, "scheduler: client authenticated");
    vec![ok_msg(id, true, "")]
}

// ---------------------------------------------------------------------------
// EVENT handler — accept future-dated events (requires AUTH)
// ---------------------------------------------------------------------------

async fn handle_event(
    arr: &[Value],
    state: &SchedulerState,
    conn: &ConnectionState,
) -> Vec<String> {
    let auth_pubkey = match &conn.auth_pubkey {
        Some(pk) => pk.clone(),
        None => {
            return vec![notice(
                "restricted: authentication required — send AUTH first",
            )]
        }
    };

    if arr.len() < 2 {
        return vec![notice("EVENT requires an event object")];
    }

    let event = &arr[1];

    // Extract required fields
    let id = match event.get("id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return vec![ok_msg("", false, "missing event id")],
    };

    let pubkey = match event.get("pubkey").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return vec![ok_msg(id, false, "missing pubkey")],
    };

    let created_at = match event.get("created_at").and_then(|v| v.as_i64()) {
        Some(ts) => ts,
        None => return vec![ok_msg(id, false, "missing or invalid created_at")],
    };

    let kind = match event.get("kind").and_then(|v| v.as_i64()) {
        Some(k) => k,
        None => return vec![ok_msg(id, false, "missing or invalid kind")],
    };

    let content = event
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let sig = match event.get("sig").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return vec![ok_msg(id, false, "missing signature")],
    };

    let tags = event
        .get("tags")
        .cloned()
        .unwrap_or_else(|| Value::Array(vec![]));

    // --- Validation ---

    // 1. Authenticated pubkey must own this event
    if pubkey != auth_pubkey {
        return vec![ok_msg(
            id,
            false,
            "restricted: event pubkey does not match authenticated pubkey",
        )];
    }

    // 2. Verify event id (sha256 of serialized event)
    if let Err(e) = verify_event_id(id, pubkey, created_at, kind, &tags, content) {
        return vec![ok_msg(id, false, &e)];
    }

    // 3. Verify Schnorr signature
    if let Err(e) = verify_signature(id, pubkey, sig) {
        return vec![ok_msg(id, false, &e)];
    }

    // 4. Kind-5 (NIP-09 deletion) — cancel the referenced scheduled events
    if kind == 5 {
        return handle_deletion(id, pubkey, &tags, state).await;
    }

    // 5. Must be in the future
    let now = Utc::now().timestamp();
    if created_at <= now {
        return vec![ok_msg(
            id,
            false,
            "invalid: created_at must be in the future. This relay only accepts future-dated events.",
        )];
    }

    // 6. Must be at least MIN_FUTURE_SECS ahead
    if created_at - now < MIN_FUTURE_SECS {
        return vec![ok_msg(
            id,
            false,
            &format!("invalid: created_at must be at least {MIN_FUTURE_SECS} seconds in the future"),
        )];
    }

    // 7. Must not be too far in the future
    if created_at - now > MAX_FUTURE_SECS {
        return vec![ok_msg(
            id,
            false,
            "invalid: created_at must be within 90 days from now",
        )];
    }

    // 8. Check per-pubkey limit
    let pending_count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM scheduled_events WHERE pubkey = $1 AND status = 'pending'",
    )
    .bind(pubkey)
    .fetch_one(&state.pool)
    .await
    .unwrap_or((0,));

    if pending_count.0 >= MAX_PENDING_PER_PUBKEY {
        return vec![ok_msg(
            id,
            false,
            &format!("rate-limited: maximum {MAX_PENDING_PER_PUBKEY} pending scheduled events per pubkey"),
        )];
    }

    // 9. Check for duplicate
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM scheduled_events WHERE id = $1)")
            .bind(id)
            .fetch_one(&state.pool)
            .await
            .unwrap_or(false);

    if exists {
        return vec![ok_msg(id, true, "duplicate: event already scheduled")];
    }

    // --- Store ---
    let tags_json = serde_json::to_value(&tags).unwrap_or(Value::Array(vec![]));

    let result = sqlx::query(
        r#"
        INSERT INTO scheduled_events (id, pubkey, kind, created_at, content, tags, sig, raw)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        ON CONFLICT (id) DO NOTHING
        "#,
    )
    .bind(id)
    .bind(pubkey)
    .bind(kind as i32)
    .bind(created_at)
    .bind(content)
    .bind(&tags_json)
    .bind(sig)
    .bind(event)
    .execute(&state.pool)
    .await;

    match result {
        Ok(_) => {
            let scheduled_dt = chrono::DateTime::from_timestamp(created_at, 0)
                .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                .unwrap_or_else(|| created_at.to_string());

            tracing::info!(
                event_id = %id,
                pubkey = %pubkey,
                kind = kind,
                scheduled_for = %scheduled_dt,
                "event scheduled"
            );
            vec![ok_msg(id, true, &format!("scheduled for {scheduled_dt}"))]
        }
        Err(e) => {
            tracing::error!(event_id = %id, error = %e, "failed to store scheduled event");
            vec![ok_msg(id, false, "error: failed to store event")]
        }
    }
}

// ---------------------------------------------------------------------------
// Deletion handler — cancel scheduled events via kind-5 (NIP-09)
// ---------------------------------------------------------------------------

async fn handle_deletion(
    del_event_id: &str,
    pubkey: &str,
    tags: &Value,
    state: &SchedulerState,
) -> Vec<String> {
    let event_ids: Vec<String> = tags
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|tag| {
                    let t = tag.as_array()?;
                    if t.first()?.as_str() == Some("e") {
                        Some(t.get(1)?.as_str()?.to_string())
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    if event_ids.is_empty() {
        return vec![ok_msg(
            del_event_id,
            false,
            "deletion event has no 'e' tags",
        )];
    }

    // Only cancel pending events that belong to the authenticated pubkey.
    // Events in any other status (publishing/published/failed) are untouched.
    let result = sqlx::query(
        r#"
        UPDATE scheduled_events
        SET status = 'cancelled'
        WHERE id = ANY($1)
          AND pubkey = $2
          AND status = 'pending'
        "#,
    )
    .bind(&event_ids)
    .bind(pubkey)
    .execute(&state.pool)
    .await;

    match result {
        Ok(r) => {
            let cancelled = r.rows_affected();
            tracing::info!(
                pubkey = %pubkey,
                cancelled = cancelled,
                "scheduler: events cancelled"
            );
            vec![ok_msg(
                del_event_id,
                true,
                &format!("cancelled {cancelled} scheduled event(s)"),
            )]
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to cancel scheduled events");
            vec![ok_msg(del_event_id, false, "error: failed to cancel events")]
        }
    }
}

// ---------------------------------------------------------------------------
// REQ handler — query own scheduled events (requires AUTH)
// ---------------------------------------------------------------------------

async fn handle_req(arr: &[Value], state: &SchedulerState, conn: &ConnectionState) -> Vec<String> {
    let auth_pubkey = match &conn.auth_pubkey {
        Some(pk) => pk.clone(),
        None => {
            return vec![notice(
                "restricted: authentication required — send AUTH first",
            )]
        }
    };

    if arr.len() < 3 {
        return vec![notice("REQ requires subscription_id and at least one filter")];
    }

    let sub_id = match arr[1].as_str() {
        Some(s) => s.to_string(),
        None => return vec![notice("subscription_id must be a string")],
    };

    let filter = &arr[2];
    let limit = filter
        .get("limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(50)
        .clamp(1, 200);

    // Always query ONLY the authenticated pubkey regardless of any authors filter.
    // This is the critical isolation guarantee — clients can never retrieve another
    // pubkey's scheduled events, even by manipulating the filter.
    let rows = sqlx::query_as::<_, ScheduledEventRow>(
        r#"
        SELECT id, pubkey, kind, created_at, content, tags, sig, raw, status,
               relays_sent, relays_failed, submitted_at, published_at, error_message
        FROM scheduled_events
        WHERE pubkey = $1
          AND status != 'cancelled'
        ORDER BY created_at ASC
        LIMIT $2
        "#,
    )
    .bind(&auth_pubkey)
    .bind(limit)
    .fetch_all(&state.pool)
    .await;

    let mut messages = Vec::new();

    match rows {
        Ok(events) => {
            for event in events {
                // Return the raw event with scheduling metadata in a custom tag
                let mut raw = event.raw.0.clone();
                if let Some(obj) = raw.as_object_mut() {
                    obj.insert(
                        "_scheduler".to_string(),
                        serde_json::json!({
                            "status": event.status,
                            "submitted_at": event.submitted_at.to_rfc3339(),
                            "published_at": event.published_at.map(|dt| dt.to_rfc3339()),
                            "relays_sent": event.relays_sent.0,
                            "relays_failed": event.relays_failed.0,
                            "error": event.error_message,
                        }),
                    );
                }
                let msg = serde_json::to_string(&serde_json::json!(["EVENT", sub_id, raw]))
                    .expect("json serialization cannot fail");
                messages.push(msg);
            }
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to query scheduled events");
            messages.push(notice("error: failed to query scheduled events"));
        }
    }

    messages.push(eose(&sub_id));
    messages
}

#[derive(sqlx::FromRow)]
#[allow(dead_code)]
struct ScheduledEventRow {
    id: String,
    pubkey: String,
    kind: i32,
    created_at: i64,
    content: String,
    tags: sqlx::types::Json<Value>,
    sig: String,
    raw: sqlx::types::Json<Value>,
    status: String,
    relays_sent: sqlx::types::Json<Value>,
    relays_failed: sqlx::types::Json<Value>,
    submitted_at: chrono::DateTime<Utc>,
    published_at: Option<chrono::DateTime<Utc>>,
    error_message: Option<String>,
}

fn handle_close(arr: &[Value]) -> Vec<String> {
    if arr.len() < 2 {
        return vec![notice("CLOSE requires subscription_id")];
    }
    let sub_id = arr[1].as_str().unwrap_or("unknown");
    vec![closed(sub_id, "")]
}

// ---------------------------------------------------------------------------
// Background publisher — runs every 60 seconds
// ---------------------------------------------------------------------------

pub async fn run_publisher(state: SchedulerState, mut shutdown_rx: broadcast::Receiver<()>) {
    // Initial delay to let things stabilize
    tokio::time::sleep(Duration::from_secs(5)).await;

    let mut interval = tokio::time::interval(Duration::from_secs(60));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = shutdown_rx.recv() => {
                tracing::info!("scheduler publisher shutting down");
                return;
            }
        }

        if let Err(e) = publish_due_events(&state).await {
            tracing::error!(error = %e, "scheduler publish cycle failed");
        }
    }
}

async fn publish_due_events(state: &SchedulerState) -> Result<(), Box<dyn std::error::Error>> {
    let now = Utc::now().timestamp();

    // Claim events that are due — atomically set status to 'publishing' to prevent double-sends
    let due_events = sqlx::query_as::<_, ScheduledEventRow>(
        r#"
        UPDATE scheduled_events
        SET status = 'publishing'
        WHERE id IN (
            SELECT id FROM scheduled_events
            WHERE status = 'pending' AND created_at <= $1
            ORDER BY created_at ASC
            LIMIT 50
            FOR UPDATE SKIP LOCKED
        )
        RETURNING id, pubkey, kind, created_at, content, tags, sig, raw, status,
                  relays_sent, relays_failed, submitted_at, published_at, error_message
        "#,
    )
    .bind(now)
    .fetch_all(&state.pool)
    .await?;

    if due_events.is_empty() {
        return Ok(());
    }

    tracing::info!(count = due_events.len(), "publishing scheduled events");

    // Collect unique pubkeys to batch-fetch relay lists
    let pubkeys: Vec<String> = due_events
        .iter()
        .map(|e| e.pubkey.clone())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    let relay_lists = state
        .relay_router
        .get_batch_author_relays(&pubkeys)
        .await
        .unwrap_or_default();

    // Get fallback relays (cached top relays)
    let fallback_relays = state.top_relays.read().await.clone();

    for event in due_events {
        let write_relays = get_write_relays_for_pubkey(
            &event.pubkey,
            &relay_lists,
            &fallback_relays,
        );

        let raw_json = serde_json::to_string(&serde_json::json!(["EVENT", event.raw.0]))
            .expect("json serialization cannot fail");

        let (sent, failed) = publish_to_relays(&raw_json, &write_relays).await;

        let status = if !sent.is_empty() { "published" } else { "failed" };
        let error_msg = if sent.is_empty() {
            Some("failed to publish to any relay".to_string())
        } else {
            None
        };

        tracing::info!(
            event_id = %event.id,
            pubkey = %event.pubkey,
            relays_sent = sent.len(),
            relays_failed = failed.len(),
            status = status,
            "event published"
        );

        let _ = sqlx::query(
            r#"
            UPDATE scheduled_events
            SET status = $1,
                relays_sent = $2,
                relays_failed = $3,
                published_at = NOW(),
                error_message = $4
            WHERE id = $5
            "#,
        )
        .bind(status)
        .bind(serde_json::json!(sent))
        .bind(serde_json::json!(failed))
        .bind(&error_msg)
        .bind(&event.id)
        .execute(&state.pool)
        .await;
    }

    Ok(())
}

/// Determine write relays for a pubkey. Falls back to top relays if no NIP-65 list found.
fn get_write_relays_for_pubkey(
    pubkey: &str,
    relay_lists: &HashMap<String, Vec<crate::crawler::relay_router::RelayPreference>>,
    fallback_relays: &[String],
) -> Vec<String> {
    if let Some(prefs) = relay_lists.get(pubkey) {
        let write_relays: Vec<String> = prefs
            .iter()
            .filter(|p| p.write)
            .map(|p| p.url.clone())
            .collect();

        if !write_relays.is_empty() {
            tracing::debug!(pubkey = %pubkey, relays = write_relays.len(), "using NIP-65 write relays");
            return write_relays;
        }
    }

    tracing::debug!(pubkey = %pubkey, relays = fallback_relays.len(), "no NIP-65 relay list, using fallback top relays");
    fallback_relays.to_vec()
}

/// Publish a raw EVENT message to multiple relays concurrently.
/// Returns (successfully_sent_urls, failed_urls).
async fn publish_to_relays(raw_event_msg: &str, relays: &[String]) -> (Vec<String>, Vec<String>) {
    let mut sent = Vec::new();
    let mut failed = Vec::new();

    // Process in batches of PUBLISH_CONCURRENCY
    for chunk in relays.chunks(PUBLISH_CONCURRENCY) {
        let mut handles = Vec::new();

        for relay_url in chunk {
            let url = relay_url.clone();
            let msg = raw_event_msg.to_string();

            handles.push(tokio::spawn(async move {
                match tokio::time::timeout(RELAY_SEND_TIMEOUT, send_to_relay(&url, &msg)).await {
                    Ok(Ok(())) => Ok(url),
                    Ok(Err(e)) => {
                        tracing::debug!(relay = %url, error = %e, "failed to send to relay");
                        Err(url)
                    }
                    Err(_) => {
                        tracing::debug!(relay = %url, "relay send timed out");
                        Err(url)
                    }
                }
            }));
        }

        for handle in handles {
            match handle.await {
                Ok(Ok(url)) => sent.push(url),
                Ok(Err(url)) => failed.push(url),
                Err(_) => {} // task panicked
            }
        }
    }

    (sent, failed)
}

/// Connect to a relay, send the EVENT message, wait for OK response.
async fn send_to_relay(relay_url: &str, event_msg: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (mut ws_stream, _) = tokio_tungstenite::connect_async(relay_url).await?;

    ws_stream
        .send(tungstenite::Message::Text(event_msg.to_string().into()))
        .await?;

    // Wait for OK response (with timeout handled by caller)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(5), ws_stream.next()).await {
            Ok(Some(Ok(tungstenite::Message::Text(text)))) => {
                if let Ok(parsed) = serde_json::from_str::<Value>(&text) {
                    if parsed.get(0).and_then(|v| v.as_str()) == Some("OK") {
                        let accepted = parsed.get(2).and_then(|v| v.as_bool()).unwrap_or(false);
                        if accepted {
                            let _ = ws_stream.close(None).await;
                            return Ok(());
                        } else {
                            let reason = parsed.get(3).and_then(|v| v.as_str()).unwrap_or("unknown");
                            let _ = ws_stream.close(None).await;
                            return Err(format!("relay rejected: {reason}").into());
                        }
                    }
                    // Not an OK message, continue waiting
                }
            }
            Ok(Some(Ok(_))) => continue, // Non-text message
            Ok(Some(Err(e))) => return Err(Box::new(e)),
            Ok(None) => return Err("connection closed".into()),
            Err(_) => {
                // Timeout waiting for OK — treat as success since we sent the event
                let _ = ws_stream.close(None).await;
                return Ok(());
            }
        }
    }

    // Didn't get OK in time, but event was sent
    let _ = ws_stream.close(None).await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Top relays cache refresher
// ---------------------------------------------------------------------------

/// Periodically refresh the cached top relays list (every 6 hours).
pub async fn refresh_top_relays_loop(state: SchedulerState) {
    // Initial load
    refresh_top_relays(&state).await;

    let mut interval = tokio::time::interval(Duration::from_secs(6 * 3600));
    loop {
        interval.tick().await;
        refresh_top_relays(&state).await;
    }
}

async fn refresh_top_relays(state: &SchedulerState) {
    match state.relay_router.get_top_relays(FALLBACK_RELAY_COUNT).await {
        Ok(relays) => {
            let urls: Vec<String> = relays.into_iter().map(|(url, _)| url).collect();
            tracing::info!(count = urls.len(), "refreshed top relays cache for scheduler");
            *state.top_relays.write().await = urls;
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to refresh top relays cache");
        }
    }
}

// ---------------------------------------------------------------------------
// Event verification
// ---------------------------------------------------------------------------

/// Verify the event id matches the sha256 of the serialized event.
/// Per NIP-01: sha256(json([0, pubkey, created_at, kind, tags, content]))
fn verify_event_id(
    id: &str,
    pubkey: &str,
    created_at: i64,
    kind: i64,
    tags: &Value,
    content: &str,
) -> Result<(), String> {
    let serialized = serde_json::to_string(&serde_json::json!([
        0, pubkey, created_at, kind, tags, content
    ]))
    .map_err(|e| format!("serialization error: {e}"))?;

    let mut hasher = Sha256::new();
    hasher.update(serialized.as_bytes());
    let hash = hex::encode(hasher.finalize());

    if hash != id {
        return Err(format!("invalid: event id mismatch (expected {hash})"));
    }

    Ok(())
}

/// Verify the Schnorr signature on the event.
fn verify_signature(event_id: &str, pubkey: &str, sig: &str) -> Result<(), String> {
    use secp256k1::{Secp256k1, XOnlyPublicKey};

    let secp = Secp256k1::verification_only();

    let msg_bytes =
        hex::decode(event_id).map_err(|_| "invalid: event id is not valid hex".to_string())?;
    let msg = secp256k1::Message::from_digest_slice(&msg_bytes)
        .map_err(|_| "invalid: event id is not a valid 32-byte hash".to_string())?;

    let pk_bytes =
        hex::decode(pubkey).map_err(|_| "invalid: pubkey is not valid hex".to_string())?;
    let xonly = XOnlyPublicKey::from_slice(&pk_bytes)
        .map_err(|_| "invalid: pubkey is not a valid x-only public key".to_string())?;

    let sig_bytes =
        hex::decode(sig).map_err(|_| "invalid: signature is not valid hex".to_string())?;
    let schnorr_sig = secp256k1::schnorr::Signature::from_slice(&sig_bytes)
        .map_err(|_| "invalid: signature is not a valid Schnorr signature".to_string())?;

    secp.verify_schnorr(&schnorr_sig, &msg, &xonly)
        .map_err(|_| "invalid: signature verification failed".to_string())?;

    Ok(())
}

// ---------------------------------------------------------------------------
// NIP-01 message formatting
// ---------------------------------------------------------------------------

/// Format an OK message (NIP-20).
fn ok_msg(event_id: &str, accepted: bool, message: &str) -> String {
    serde_json::to_string(&serde_json::json!(["OK", event_id, accepted, message]))
        .expect("json serialization cannot fail")
}

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
