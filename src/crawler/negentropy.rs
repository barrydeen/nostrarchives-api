use futures_util::{SinkExt, StreamExt};
use negentropy::{Id, Negentropy, NegentropyStorageVector};
use sqlx::PgPool;
use tokio::time::{timeout, Duration, Instant};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::cache::StatsCache;
use crate::db::models::NostrEvent;
use crate::db::repository::EventRepository;
use crate::error::AppError;

/// Result of a negentropy set-reconciliation round.
pub struct SyncResult {
    /// Hex event IDs we have that the relay doesn't.
    pub have_ids: Vec<String>,
    /// Hex event IDs the relay has that we don't.
    pub need_ids: Vec<String>,
}

/// Statistics from a full sync cycle.
pub struct SyncStats {
    pub relay_url: String,
    pub events_discovered: usize,
    pub events_fetched: usize,
    pub events_inserted: usize,
    pub duration_ms: u64,
}

/// Negentropy-based sync engine.
pub struct NegentropySyncer {
    repo: EventRepository,
    cache: StatsCache,
    pool: PgPool,
}

impl NegentropySyncer {
    pub fn new(repo: EventRepository, cache: StatsCache, pool: PgPool) -> Self {
        Self { repo, cache, pool }
    }

    // -----------------------------------------------------------------------
    // Core negentropy reconciliation
    // -----------------------------------------------------------------------

    /// Run negentropy set-reconciliation against a relay for the given event kinds.
    /// Returns IDs the relay has that we don't (need_ids) and vice versa (have_ids).
    pub async fn sync_with_relay(
        &self,
        relay_url: &str,
        kinds: &[i64],
    ) -> Result<SyncResult, AppError> {
        self.sync_with_relay_window(relay_url, kinds, None, None).await
    }

    /// Windowed negentropy reconciliation. Constrains both the relay filter and local
    /// DB query to `[since, until)` so relays don't reject for "too many results".
    pub async fn sync_with_relay_window(
        &self,
        relay_url: &str,
        kinds: &[i64],
        since: Option<i64>,
        until: Option<i64>,
    ) -> Result<SyncResult, AppError> {
        // 1. Load local events matching the requested kinds + time window
        let rows = if since.is_some() || until.is_some() {
            let s = since.unwrap_or(0);
            let u = until.unwrap_or(i64::MAX);
            sqlx::query_as::<_, EventIdRow>(
                "SELECT id, created_at FROM events WHERE kind = ANY($1) AND created_at >= $2 AND created_at < $3 ORDER BY created_at ASC, id ASC",
            )
            .bind(kinds)
            .bind(s)
            .bind(u)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, EventIdRow>(
                "SELECT id, created_at FROM events WHERE kind = ANY($1) ORDER BY created_at ASC, id ASC",
            )
            .bind(kinds)
            .fetch_all(&self.pool)
            .await?
        };

        // 2. Build negentropy storage
        let mut storage = NegentropyStorageVector::with_capacity(rows.len());
        for row in &rows {
            let id_bytes = hex::decode(&row.id)
                .map_err(|e| AppError::Internal(format!("hex decode event id: {e}")))?;
            let neg_id = Id::from_slice(&id_bytes)
                .map_err(|e| AppError::Internal(format!("neg Id: {e}")))?;
            storage
                .insert(row.created_at as u64, neg_id)
                .map_err(|e| AppError::Internal(format!("neg insert: {e}")))?;
        }
        storage
            .seal()
            .map_err(|e| AppError::Internal(format!("neg seal: {e}")))?;

        tracing::info!(
            relay = relay_url,
            local_events = rows.len(),
            "starting negentropy reconciliation"
        );

        // 3. Create negentropy instance and initiate
        let mut neg = Negentropy::owned(storage, 131_072)
            .map_err(|e| AppError::Internal(format!("neg new: {e}")))?;
        let init_msg = neg
            .initiate()
            .map_err(|e| AppError::Internal(format!("neg initiate: {e}")))?;

        // 4. Connect WebSocket
        let (mut ws, _) = timeout(Duration::from_secs(10), connect_async(relay_url))
            .await
            .map_err(|_| AppError::Internal(format!("WS connect timeout: {relay_url}")))?
            .map_err(|e| AppError::Internal(format!("WS connect {relay_url}: {e}")))?;

        // 5. Send NEG-OPEN
        let sub_id = format!("neg-{}", uuid::Uuid::new_v4().as_simple());
        let kinds_json: Vec<serde_json::Value> = kinds.iter().map(|&k| serde_json::json!(k)).collect();
        let mut filter = serde_json::json!({"kinds": kinds_json});
        if let Some(s) = since {
            filter["since"] = serde_json::json!(s);
        }
        if let Some(u) = until {
            filter["until"] = serde_json::json!(u);
        }
        let neg_open =
            serde_json::json!(["NEG-OPEN", &sub_id, filter, hex::encode(&init_msg)]);

        ws.send(Message::Text(neg_open.to_string().into()))
            .await
            .map_err(|e| AppError::Internal(format!("WS send NEG-OPEN: {e}")))?;

        // 6. Reconciliation loop
        let mut all_have_ids: Vec<Id> = Vec::new();
        let mut all_need_ids: Vec<Id> = Vec::new();

        let deadline = Instant::now() + Duration::from_secs(30);

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(AppError::Internal(format!(
                    "negentropy reconciliation timeout: {relay_url}"
                )));
            }

            let msg = match timeout(remaining, ws.next()).await {
                Ok(Some(Ok(msg))) => msg,
                Ok(Some(Err(e))) => {
                    return Err(AppError::Internal(format!("WS recv error: {e}")));
                }
                Ok(None) => {
                    return Err(AppError::Internal("WS closed unexpectedly".to_string()));
                }
                Err(_) => {
                    return Err(AppError::Internal(format!(
                        "negentropy reconciliation timeout: {relay_url}"
                    )));
                }
            };

            let txt = match msg {
                Message::Text(t) => t,
                Message::Ping(_) | Message::Pong(_) => continue,
                Message::Close(_) => {
                    return Err(AppError::Internal("WS closed by relay".to_string()));
                }
                _ => continue,
            };

            let txt_str: &str = txt.as_ref();
            let arr: Vec<serde_json::Value> = serde_json::from_str(txt_str)
                .map_err(|e| AppError::Internal(format!("JSON parse: {e}")))?;

            let cmd = arr
                .first()
                .and_then(|v| v.as_str())
                .unwrap_or_default();

            match cmd {
                "NEG-MSG" => {
                    let msg_hex = arr
                        .get(2)
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| AppError::Internal("NEG-MSG missing hex".to_string()))?;

                    let msg_bytes = hex::decode(msg_hex)
                        .map_err(|e| AppError::Internal(format!("hex decode NEG-MSG: {e}")))?;

                    let mut have_ids: Vec<Id> = Vec::new();
                    let mut need_ids: Vec<Id> = Vec::new();

                    let next = neg
                        .reconcile_with_ids(&msg_bytes, &mut have_ids, &mut need_ids)
                        .map_err(|e| AppError::Internal(format!("neg reconcile: {e}")))?;

                    all_have_ids.extend(have_ids);
                    all_need_ids.extend(need_ids);

                    match next {
                        Some(next_msg) => {
                            let reply = serde_json::json!([
                                "NEG-MSG",
                                &sub_id,
                                hex::encode(&next_msg)
                            ]);
                            ws.send(Message::Text(reply.to_string().into()))
                                .await
                                .map_err(|e| {
                                    AppError::Internal(format!("WS send NEG-MSG: {e}"))
                                })?;
                        }
                        None => {
                            // Reconciliation complete
                            break;
                        }
                    }
                }
                "NEG-ERR" => {
                    let reason = arr
                        .get(2)
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    return Err(AppError::Internal(format!(
                        "NEG-ERR from {relay_url}: {reason}"
                    )));
                }
                "NOTICE" => {
                    let notice = arr.get(1).and_then(|v| v.as_str()).unwrap_or("");
                    tracing::warn!(relay = relay_url, "NOTICE: {notice}");
                    // Continue — some relays send notices alongside normal messages
                }
                _ => {
                    // Ignore unknown messages
                }
            }
        }

        // 7. Clean close
        let neg_close = serde_json::json!(["NEG-CLOSE", &sub_id]);
        let _ = ws.send(Message::Text(neg_close.to_string().into())).await;
        let _ = ws.close(None).await;

        // 8. Convert IDs to hex strings
        let have_hex: Vec<String> = all_have_ids.iter().map(|id| hex::encode(id.as_bytes())).collect();
        let need_hex: Vec<String> = all_need_ids.iter().map(|id| hex::encode(id.as_bytes())).collect();

        tracing::info!(
            relay = relay_url,
            have = have_hex.len(),
            need = need_hex.len(),
            "negentropy reconciliation complete"
        );

        Ok(SyncResult {
            have_ids: have_hex,
            need_ids: need_hex,
        })
    }

    // -----------------------------------------------------------------------
    // Batch fetch missing events via REQ
    // -----------------------------------------------------------------------

    /// Fetch events by ID from a relay in chunks of 500.
    pub async fn fetch_missing_events(
        &self,
        relay_url: &str,
        event_ids: &[String],
    ) -> Result<Vec<NostrEvent>, AppError> {
        if event_ids.is_empty() {
            return Ok(Vec::new());
        }

        let (mut ws, _) = timeout(Duration::from_secs(10), connect_async(relay_url))
            .await
            .map_err(|_| AppError::Internal(format!("WS connect timeout: {relay_url}")))?
            .map_err(|e| AppError::Internal(format!("WS connect {relay_url}: {e}")))?;

        let mut all_events: Vec<NostrEvent> = Vec::new();

        for (chunk_idx, chunk) in event_ids.chunks(500).enumerate() {
            let sub_id = format!("fetch-{chunk_idx}");
            let ids_json: Vec<&str> = chunk.iter().map(|s| s.as_str()).collect();
            let req = serde_json::json!(["REQ", &sub_id, {"ids": ids_json}]);

            ws.send(Message::Text(req.to_string().into()))
                .await
                .map_err(|e| AppError::Internal(format!("WS send REQ: {e}")))?;

            let deadline = Instant::now() + Duration::from_secs(30);

            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    tracing::warn!(relay = relay_url, chunk = chunk_idx, "fetch chunk timeout");
                    break;
                }

                let msg = match timeout(remaining, ws.next()).await {
                    Ok(Some(Ok(msg))) => msg,
                    Ok(Some(Err(e))) => {
                        tracing::warn!("WS recv error during fetch: {e}");
                        break;
                    }
                    Ok(None) => break,
                    Err(_) => break,
                };

                let txt = match msg {
                    Message::Text(t) => t,
                    Message::Ping(_) | Message::Pong(_) => continue,
                    Message::Close(_) => break,
                    _ => continue,
                };

                let txt_str: &str = txt.as_ref();
                let arr: Vec<serde_json::Value> = match serde_json::from_str(txt_str) {
                    Ok(a) => a,
                    Err(_) => continue,
                };

                let cmd = arr.first().and_then(|v| v.as_str()).unwrap_or_default();

                match cmd {
                    "EVENT" => {
                        if let Some(event_val) = arr.get(2) {
                            if let Ok(event) =
                                serde_json::from_value::<NostrEvent>(event_val.clone())
                            {
                                all_events.push(event);
                            }
                        }
                    }
                    "EOSE" => {
                        // End of stored events for this subscription
                        break;
                    }
                    "NOTICE" => {
                        let notice = arr.get(1).and_then(|v| v.as_str()).unwrap_or("");
                        tracing::warn!(relay = relay_url, "NOTICE during fetch: {notice}");
                    }
                    _ => {}
                }
            }

            // Close this subscription
            let close_msg = serde_json::json!(["CLOSE", &sub_id]);
            let _ = ws.send(Message::Text(close_msg.to_string().into())).await;
        }

        let _ = ws.close(None).await;

        tracing::info!(
            relay = relay_url,
            requested = event_ids.len(),
            fetched = all_events.len(),
            "batch fetch complete"
        );

        Ok(all_events)
    }

    // -----------------------------------------------------------------------
    // Full sync orchestration
    // -----------------------------------------------------------------------

    /// Default time window size for chunked sync (24 hours in seconds).
    const DEFAULT_WINDOW_SECS: i64 = 86400;

    /// Run a full negentropy sync cycle: reconcile → fetch missing → insert.
    /// Uses a single time window (unbounded). For large relays, use `run_sync_windowed`.
    pub async fn run_sync(&self, relay_url: &str) -> Result<SyncStats, AppError> {
        self.run_sync_window(relay_url, None, None).await
    }

    /// Run negentropy sync for a specific time window.
    pub async fn run_sync_window(
        &self,
        relay_url: &str,
        since: Option<i64>,
        until: Option<i64>,
    ) -> Result<SyncStats, AppError> {
        let start = Instant::now();

        // 1. Reconcile kind-1 notes
        let sync_result = self
            .sync_with_relay_window(relay_url, &[1], since, until)
            .await?;
        let events_discovered = sync_result.need_ids.len();

        // 2. Fetch missing events
        let events = self
            .fetch_missing_events(relay_url, &sync_result.need_ids)
            .await?;
        let events_fetched = events.len();

        // 3. Insert and update cache
        let mut events_inserted = 0usize;
        for event in &events {
            match self.repo.insert_event(event, relay_url).await {
                Ok(true) => {
                    events_inserted += 1;
                    self.cache
                        .on_event_ingested(&event.pubkey, event.kind)
                        .await;
                }
                Ok(false) => {
                    // Duplicate, skip
                }
                Err(e) => {
                    tracing::warn!(event_id = %event.id, "insert failed: {e}");
                }
            }
        }

        let duration_ms = start.elapsed().as_millis() as u64;

        tracing::info!(
            relay = relay_url,
            since = since.unwrap_or(0),
            until = until.unwrap_or(0),
            discovered = events_discovered,
            fetched = events_fetched,
            inserted = events_inserted,
            duration_ms = duration_ms,
            "sync window complete"
        );

        Ok(SyncStats {
            relay_url: relay_url.to_string(),
            events_discovered,
            events_fetched,
            events_inserted,
            duration_ms,
        })
    }

    /// Run negentropy sync using backward time windows.
    /// Starts from `now` and walks backward in `window_secs` chunks.
    /// Handles `NEG-ERR: blocked` by halving the window and retrying.
    /// Stops after `max_windows` windows or when a window yields zero new events.
    pub async fn run_sync_windowed(
        &self,
        relay_url: &str,
        window_secs: Option<i64>,
        max_windows: usize,
    ) -> Result<SyncStats, AppError> {
        let start = Instant::now();
        let now = chrono::Utc::now().timestamp();
        let mut window = window_secs.unwrap_or(Self::DEFAULT_WINDOW_SECS);
        let min_window: i64 = 3600; // don't go below 1 hour

        let mut total_discovered = 0usize;
        let mut total_fetched = 0usize;
        let mut total_inserted = 0usize;
        let mut cursor = now;
        let mut windows_done = 0usize;

        while windows_done < max_windows && cursor > 0 {
            let window_since = cursor - window;
            let window_until = cursor;

            tracing::info!(
                relay = relay_url,
                window = windows_done + 1,
                since = window_since,
                until = window_until,
                window_hours = window / 3600,
                "starting sync window"
            );

            match self
                .run_sync_window(relay_url, Some(window_since), Some(window_until))
                .await
            {
                Ok(stats) => {
                    total_discovered += stats.events_discovered;
                    total_fetched += stats.events_fetched;
                    total_inserted += stats.events_inserted;
                    windows_done += 1;

                    // If this window yielded nothing, no point going further back
                    if stats.events_discovered == 0 && windows_done > 1 {
                        tracing::info!(
                            relay = relay_url,
                            "empty window — stopping backward crawl"
                        );
                        break;
                    }

                    // Move cursor backward
                    cursor = window_since;
                }
                Err(e) => {
                    let err_msg = format!("{e}");
                    if err_msg.contains("too many") || err_msg.contains("blocked") {
                        // Relay rejected — halve the window and retry
                        let new_window = window / 2;
                        if new_window < min_window {
                            tracing::warn!(
                                relay = relay_url,
                                window_secs = window,
                                "window too small, skipping this range"
                            );
                            cursor = window_since;
                            windows_done += 1;
                            continue;
                        }
                        tracing::info!(
                            relay = relay_url,
                            old_window = window,
                            new_window = new_window,
                            "NEG-ERR blocked — halving window size"
                        );
                        window = new_window;
                        // Don't increment windows_done — retry same cursor
                    } else {
                        tracing::warn!(
                            relay = relay_url,
                            error = %e,
                            "sync window failed, skipping"
                        );
                        cursor = window_since;
                        windows_done += 1;
                    }
                }
            }
        }

        let duration_ms = start.elapsed().as_millis() as u64;

        tracing::info!(
            relay = relay_url,
            windows = windows_done,
            total_discovered = total_discovered,
            total_fetched = total_fetched,
            total_inserted = total_inserted,
            duration_ms = duration_ms,
            "windowed sync complete"
        );

        Ok(SyncStats {
            relay_url: relay_url.to_string(),
            events_discovered: total_discovered,
            events_fetched: total_fetched,
            events_inserted: total_inserted,
            duration_ms,
        })
    }
}

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

#[derive(sqlx::FromRow)]
struct EventIdRow {
    id: String,
    created_at: i64,
}
