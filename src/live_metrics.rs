use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::broadcast;

const BUCKET_TTL: i64 = 660; // 11 minutes
const WINDOW_MINUTES: usize = 10;
const WINDOW_MS: i64 = WINDOW_MINUTES as i64 * 60 * 1000;

/// Redis key for the sorted set tracking individual active pubkeys.
const ACTIVE_USERS_KEY: &str = "nostr:live:active_users";
/// Redis key for the hash mapping pubkey -> last activity kind.
const ACTIVE_USERS_KIND_KEY: &str = "nostr:live:active_users:activity";

fn minute_key() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M").to_string()
}

fn bucket_keys(prefix: &str) -> Vec<String> {
    let now = chrono::Utc::now();
    (0..WINDOW_MINUTES)
        .map(|i| {
            let t = now - chrono::Duration::minutes(i as i64);
            format!("nostr:live:{prefix}:{}", t.format("%Y-%m-%dT%H:%M"))
        })
        .collect()
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[derive(Clone, Debug, Serialize)]
pub struct LiveMetrics {
    pub online: i64,
    pub sats: i64,
    pub notes: i64,
}

/// A single active user update broadcast to WebSocket subscribers.
#[derive(Clone, Debug, Serialize)]
pub struct OnlineUserUpdate {
    pub pubkey: String,
    pub last_active_ms: i64,
    pub activity_kind: i64,
}

/// Snapshot entry for an active user (used in initial WS snapshot).
#[derive(Clone, Debug, Serialize)]
pub struct ActiveUser {
    pub pubkey: String,
    pub last_active_ms: i64,
    pub activity_kind: i64,
}

#[derive(Clone)]
pub struct LiveMetricsTracker {
    redis: redis::Client,
    tx: broadcast::Sender<LiveMetrics>,
    online_tx: broadcast::Sender<OnlineUserUpdate>,
}

impl LiveMetricsTracker {
    pub fn new(redis: redis::Client) -> Self {
        let (tx, _) = broadcast::channel(256);
        let (online_tx, _) = broadcast::channel(512);
        Self { redis, tx, online_tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<LiveMetrics> {
        self.tx.subscribe()
    }

    pub fn subscribe_online_users(&self) -> broadcast::Receiver<OnlineUserUpdate> {
        self.online_tx.subscribe()
    }

    /// Record an event for live metrics tracking.
    ///
    /// Called from the ingester for every successfully processed event.
    /// - All events: PFADD pubkey to online HyperLogLog for current minute
    /// - All events: ZADD pubkey to active_users sorted set with timestamp
    /// - Kind 1: INCR notes counter for current minute
    /// - Kind 9735 with zap_sats > 0: INCRBY sats counter for current minute
    ///
    /// After writing, reads a snapshot and broadcasts it to all WS subscribers.
    pub async fn record_event(&self, pubkey: &str, kind: i64, zap_sats: i64) {
        let Ok(mut conn) = self.redis.get_multiplexed_async_connection().await else {
            return;
        };

        let mk = minute_key();
        let online_key = format!("nostr:live:online:{mk}");
        let notes_key = format!("nostr:live:notes:{mk}");
        let sats_key = format!("nostr:live:sats:{mk}");
        let ts = now_ms();

        let mut pipe = redis::pipe();

        // Always track online presence (HyperLogLog for count)
        pipe.cmd("PFADD")
            .arg(&online_key)
            .arg(pubkey)
            .ignore()
            .cmd("EXPIRE")
            .arg(&online_key)
            .arg(BUCKET_TTL)
            .ignore();

        // Track individual active users (sorted set for enumeration)
        // GT flag: only update score if new score is greater (most recent wins)
        pipe.cmd("ZADD")
            .arg(ACTIVE_USERS_KEY)
            .arg("GT")
            .arg(ts)
            .arg(pubkey)
            .ignore();

        // Track most recent activity kind per user
        pipe.cmd("HSET")
            .arg(ACTIVE_USERS_KIND_KEY)
            .arg(pubkey)
            .arg(kind)
            .ignore();

        // Track kind-1 notes
        if kind == 1 {
            pipe.cmd("INCR")
                .arg(&notes_key)
                .ignore()
                .cmd("EXPIRE")
                .arg(&notes_key)
                .arg(BUCKET_TTL)
                .ignore();
        }

        // Track zap sats
        if zap_sats > 0 {
            pipe.cmd("INCRBY")
                .arg(&sats_key)
                .arg(zap_sats)
                .ignore()
                .cmd("EXPIRE")
                .arg(&sats_key)
                .arg(BUCKET_TTL)
                .ignore();
        }

        let _: Result<(), _> = pipe.query_async(&mut conn).await;

        // Broadcast per-user update
        let _ = self.online_tx.send(OnlineUserUpdate {
            pubkey: pubkey.to_string(),
            last_active_ms: ts,
            activity_kind: kind,
        });

        // Read snapshot and broadcast aggregate metrics
        if let Some(metrics) = self.snapshot().await {
            let _ = self.tx.send(metrics);
        }
    }

    /// Record zap sats only (no online/notes tracking).
    ///
    /// Called separately from `record_event` when we know the zap amount but
    /// the pubkey has already been tracked via the main `record_event` path.
    pub async fn record_zap_sats(&self, zap_sats: i64) {
        if zap_sats <= 0 {
            return;
        }

        let Ok(mut conn) = self.redis.get_multiplexed_async_connection().await else {
            return;
        };

        let mk = minute_key();
        let sats_key = format!("nostr:live:sats:{mk}");

        let _: Result<(), _> = redis::pipe()
            .cmd("INCRBY")
            .arg(&sats_key)
            .arg(zap_sats)
            .ignore()
            .cmd("EXPIRE")
            .arg(&sats_key)
            .arg(BUCKET_TTL)
            .ignore()
            .query_async(&mut conn)
            .await;

        // Broadcast updated snapshot
        if let Some(metrics) = self.snapshot().await {
            let _ = self.tx.send(metrics);
        }
    }

    /// Read all 10 minute buckets from Redis and return aggregated metrics.
    ///
    /// Uses PFCOUNT with multiple keys for the online HLL union count,
    /// and sums individual counter keys for notes and sats.
    pub async fn snapshot(&self) -> Option<LiveMetrics> {
        let Ok(mut conn) = self.redis.get_multiplexed_async_connection().await else {
            return None;
        };

        let online_keys = bucket_keys("online");
        let notes_keys = bucket_keys("notes");
        let sats_keys = bucket_keys("sats");

        // PFCOUNT supports multiple keys (union count)
        let online: i64 = {
            let mut cmd = redis::cmd("PFCOUNT");
            for k in &online_keys {
                cmd.arg(k);
            }
            cmd.query_async(&mut conn).await.unwrap_or(0)
        };

        // Sum notes across all minute buckets via pipeline
        let mut pipe = redis::pipe();
        for k in &notes_keys {
            pipe.cmd("GET").arg(k);
        }
        for k in &sats_keys {
            pipe.cmd("GET").arg(k);
        }

        let values: Vec<Option<i64>> = pipe.query_async(&mut conn).await.unwrap_or_default();

        let notes: i64 = values[..notes_keys.len()]
            .iter()
            .map(|v| v.unwrap_or(0))
            .sum();

        let sats: i64 = values[notes_keys.len()..]
            .iter()
            .map(|v| v.unwrap_or(0))
            .sum();

        Some(LiveMetrics { online, sats, notes })
    }

    /// Return up to 500 most recently active users from the sorted set.
    pub async fn active_users_snapshot(&self) -> Option<Vec<ActiveUser>> {
        let Ok(mut conn) = self.redis.get_multiplexed_async_connection().await else {
            return None;
        };

        let cutoff = now_ms() - WINDOW_MS;

        // ZRANGEBYSCORE to get all users active within the window
        let members: Vec<(String, f64)> = redis::cmd("ZRANGEBYSCORE")
            .arg(ACTIVE_USERS_KEY)
            .arg(cutoff)
            .arg("+inf")
            .arg("WITHSCORES")
            .arg("LIMIT")
            .arg(0)
            .arg(500)
            .query_async(&mut conn)
            .await
            .unwrap_or_default();

        if members.is_empty() {
            return Some(Vec::new());
        }

        // Batch fetch activity kinds
        let pubkeys: Vec<&str> = members.iter().map(|(pk, _)| pk.as_str()).collect();
        let mut hmget = redis::cmd("HMGET");
        hmget.arg(ACTIVE_USERS_KIND_KEY);
        for pk in &pubkeys {
            hmget.arg(*pk);
        }
        let kinds: Vec<Option<i64>> = hmget.query_async(&mut conn).await.unwrap_or_default();

        let mut users: Vec<ActiveUser> = members
            .into_iter()
            .enumerate()
            .map(|(i, (pubkey, score))| ActiveUser {
                pubkey,
                last_active_ms: score as i64,
                activity_kind: kinds.get(i).copied().flatten().unwrap_or(1),
            })
            .collect();

        // Sort by most recent first (ZRANGEBYSCORE returns ascending)
        users.sort_by(|a, b| b.last_active_ms.cmp(&a.last_active_ms));

        Some(users)
    }

    /// Remove stale entries from the active users sorted set and hash.
    /// Called periodically from a background task.
    pub async fn cleanup_active_users(&self) {
        let Ok(mut conn) = self.redis.get_multiplexed_async_connection().await else {
            return;
        };

        let cutoff = now_ms() - (BUCKET_TTL * 1000); // 660s in ms

        // Get stale pubkeys before removing (so we can clean up the hash)
        let stale: Vec<String> = redis::cmd("ZRANGEBYSCORE")
            .arg(ACTIVE_USERS_KEY)
            .arg("-inf")
            .arg(cutoff)
            .query_async(&mut conn)
            .await
            .unwrap_or_default();

        if stale.is_empty() {
            return;
        }

        let mut pipe = redis::pipe();

        // Remove from sorted set
        pipe.cmd("ZREMRANGEBYSCORE")
            .arg(ACTIVE_USERS_KEY)
            .arg("-inf")
            .arg(cutoff)
            .ignore();

        // Remove from activity hash
        let mut hdel = pipe.cmd("HDEL");
        hdel.arg(ACTIVE_USERS_KIND_KEY);
        for pk in &stale {
            hdel.arg(pk);
        }
        hdel.ignore();

        let _: Result<(), _> = pipe.query_async(&mut conn).await;

        tracing::debug!(removed = stale.len(), "cleaned up stale active users");
    }
}

/// WebSocket handler — connects to the live metrics broadcast channel.
///
/// Implemented in `api/mod.rs` because the handler needs access to `AppState`
/// which is defined in the binary crate, not the library.
///
/// Protocol:
/// 1. On connect, sends an initial snapshot as JSON
/// 2. Streams debounced updates (max 1/sec) as JSON text frames
/// 3. Sends ping every 30s for keepalive
/// 4. JSON format: `{"online": N, "sats": N, "notes": N}`
pub use self::ws_support::*;

mod ws_support {
    use super::*;

    /// Handle a live metrics WebSocket connection.
    ///
    /// Call this from your axum WS upgrade handler, passing an already-upgraded
    /// `WebSocket` and a reference to the tracker.
    pub async fn handle_live_metrics_ws(
        socket: axum::extract::ws::WebSocket,
        tracker: std::sync::Arc<LiveMetricsTracker>,
    ) {
        use axum::extract::ws::Message;
        use futures_util::{SinkExt, StreamExt};

        let (mut sender, mut receiver) = socket.split();

        // Send initial snapshot
        if let Some(metrics) = tracker.snapshot().await {
            if let Ok(json) = serde_json::to_string(&metrics) {
                if sender.send(Message::Text(json.into())).await.is_err() {
                    return;
                }
            }
        }

        let mut rx = tracker.subscribe();
        let mut last_sent = Instant::now();

        loop {
            tokio::select! {
                // Receive broadcast updates (debounce to 1 msg/sec)
                result = rx.recv() => {
                    match result {
                        Ok(metrics) => {
                            if last_sent.elapsed() >= Duration::from_secs(1) {
                                if let Ok(json) = serde_json::to_string(&metrics) {
                                    if sender.send(Message::Text(json.into())).await.is_err() {
                                        break;
                                    }
                                    last_sent = Instant::now();
                                }
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
                // Client messages (handle close / disconnect)
                msg = receiver.next() => {
                    match msg {
                        Some(Ok(Message::Close(_))) | None => break,
                        _ => {}
                    }
                }
                // Keepalive ping every 30s
                _ = tokio::time::sleep(Duration::from_secs(30)) => {
                    if sender.send(Message::Ping(vec![].into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    }

    /// Handle an online users WebSocket connection.
    ///
    /// Protocol:
    /// 1. On connect, sends full snapshot: `{"type":"snapshot","users":[...]}`
    /// 2. Streams batched updates (max 2/sec): `{"type":"update","users":[...]}`
    /// 3. Sends ping every 30s for keepalive
    pub async fn handle_online_users_ws(
        socket: axum::extract::ws::WebSocket,
        tracker: std::sync::Arc<LiveMetricsTracker>,
    ) {
        use axum::extract::ws::Message;
        use futures_util::{SinkExt, StreamExt};

        let (mut sender, mut receiver) = socket.split();

        // Send initial snapshot
        if let Some(users) = tracker.active_users_snapshot().await {
            let msg = serde_json::json!({
                "type": "snapshot",
                "users": users,
            });
            if let Ok(json) = serde_json::to_string(&msg) {
                if sender.send(Message::Text(json.into())).await.is_err() {
                    return;
                }
            }
        }

        let mut rx = tracker.subscribe_online_users();
        let mut last_sent = Instant::now();
        let mut batch: Vec<OnlineUserUpdate> = Vec::new();

        loop {
            tokio::select! {
                result = rx.recv() => {
                    match result {
                        Ok(update) => {
                            batch.push(update);

                            // Debounce: send at most 2 messages/sec
                            if last_sent.elapsed() >= Duration::from_millis(500) && !batch.is_empty() {
                                let msg = serde_json::json!({
                                    "type": "update",
                                    "users": batch,
                                });
                                batch = Vec::new();
                                if let Ok(json) = serde_json::to_string(&msg) {
                                    if sender.send(Message::Text(json.into())).await.is_err() {
                                        break;
                                    }
                                    last_sent = Instant::now();
                                }
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
                // Flush any pending batch after 500ms of no new messages
                _ = tokio::time::sleep(Duration::from_millis(500)) => {
                    if !batch.is_empty() {
                        let msg = serde_json::json!({
                            "type": "update",
                            "users": batch,
                        });
                        batch = Vec::new();
                        if let Ok(json) = serde_json::to_string(&msg) {
                            if sender.send(Message::Text(json.into())).await.is_err() {
                                break;
                            }
                            last_sent = Instant::now();
                        }
                    } else if last_sent.elapsed() >= Duration::from_secs(30) {
                        // Keepalive ping
                        if sender.send(Message::Ping(vec![].into())).await.is_err() {
                            break;
                        }
                        last_sent = Instant::now();
                    }
                }
                // Client messages (handle close / disconnect)
                msg = receiver.next() => {
                    match msg {
                        Some(Ok(Message::Close(_))) | None => break,
                        _ => {}
                    }
                }
            }
        }
    }
}
