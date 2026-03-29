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
        self.sync_with_relay_filtered(relay_url, kinds, None, since, until).await
    }

    /// Per-author negentropy reconciliation. Syncs all events of the given kinds
    /// for specific authors against a relay. No time window needed — the author
    /// filter keeps the set small enough.
    pub async fn sync_authors_with_relay(
        &self,
        relay_url: &str,
        kinds: &[i64],
        authors: &[String],
    ) -> Result<SyncResult, AppError> {
        self.sync_with_relay_filtered(relay_url, kinds, Some(authors), None, None).await
    }

    /// Core negentropy reconciliation with optional author and time filters.
    async fn sync_with_relay_filtered(
        &self,
        relay_url: &str,
        kinds: &[i64],
        authors: Option<&[String]>,
        since: Option<i64>,
        until: Option<i64>,
    ) -> Result<SyncResult, AppError> {
        // 1. Load local events matching the requested filters
        let rows = match (authors, since.is_some() || until.is_some()) {
            (Some(pks), true) => {
                let s = since.unwrap_or(0);
                let u = until.unwrap_or(i64::MAX);
                sqlx::query_as::<_, EventIdRow>(
                    "SELECT id, created_at FROM events WHERE kind = ANY($1) AND pubkey = ANY($2) AND created_at >= $3 AND created_at < $4 ORDER BY created_at ASC, id ASC",
                )
                .bind(kinds)
                .bind(pks)
                .bind(s)
                .bind(u)
                .fetch_all(&self.pool)
                .await?
            }
            (Some(pks), false) => {
                sqlx::query_as::<_, EventIdRow>(
                    "SELECT id, created_at FROM events WHERE kind = ANY($1) AND pubkey = ANY($2) ORDER BY created_at ASC, id ASC",
                )
                .bind(kinds)
                .bind(pks)
                .fetch_all(&self.pool)
                .await?
            }
            (None, true) => {
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
            }
            (None, false) => {
                sqlx::query_as::<_, EventIdRow>(
                    "SELECT id, created_at FROM events WHERE kind = ANY($1) ORDER BY created_at ASC, id ASC",
                )
                .bind(kinds)
                .fetch_all(&self.pool)
                .await?
            }
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
            authors = authors.map_or(0, |a| a.len()),
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
        if let Some(pks) = authors {
            filter["authors"] = serde_json::json!(pks);
        }
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
                    // If the notice indicates negentropy is disabled/unsupported, treat as fatal
                    let notice_lower = notice.to_lowercase();
                    if notice_lower.contains("negentropy")
                        || notice_lower.contains("neg-open")
                        || notice_lower.contains("bad msg")
                    {
                        return Err(AppError::Internal(format!(
                            "relay {relay_url} does not support negentropy: {notice}"
                        )));
                    }
                    // Otherwise continue — some relays send benign notices
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
    pub const DEFAULT_WINDOW_SECS: i64 = 86400;

    /// v2: Only sync kinds we actually store. Reactions/reposts (6/7/16) are
    /// handled as counter increments via targeted #e queries, not negentropy bulk sync.
    pub const ALL_CRAWL_KINDS: &'static [i64] = &[0, 1, 3, 9735, 10002];

    /// Run negentropy sync for a specific time window and set of kinds.
    pub async fn run_sync_window(
        &self,
        relay_url: &str,
        kinds: &[i64],
        since: Option<i64>,
        until: Option<i64>,
    ) -> Result<SyncStats, AppError> {
        let start = Instant::now();

        let sync_result = self
            .sync_with_relay_window(relay_url, kinds, since, until)
            .await?;
        let events_discovered = sync_result.need_ids.len();

        let events = self
            .fetch_missing_events(relay_url, &sync_result.need_ids)
            .await?;
        let events_fetched = events.len();

        let mut events_inserted = 0usize;
        for event in &events {
            match self.repo.insert_event(event, relay_url).await {
                Ok(true) => {
                    events_inserted += 1;
                    self.cache
                        .on_event_ingested(&event.pubkey, event.kind)
                        .await;
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(event_id = %event.id, "insert failed: {e}");
                }
            }
        }

        let duration_ms = start.elapsed().as_millis() as u64;

        tracing::info!(
            relay = relay_url,
            kinds = ?kinds,
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

    /// Per-author negentropy sync: reconcile + fetch + insert for specific authors.
    /// No time window needed — the author filter keeps the set manageable.
    pub async fn sync_authors(
        &self,
        relay_url: &str,
        kinds: &[i64],
        authors: &[String],
    ) -> Result<SyncStats, AppError> {
        let start = Instant::now();

        let sync_result = self
            .sync_authors_with_relay(relay_url, kinds, authors)
            .await?;
        let events_discovered = sync_result.need_ids.len();

        let events = self
            .fetch_missing_events(relay_url, &sync_result.need_ids)
            .await?;
        let events_fetched = events.len();

        let mut events_inserted = 0usize;
        for event in &events {
            match self.repo.insert_event(event, relay_url).await {
                Ok(true) => {
                    events_inserted += 1;
                    self.cache
                        .on_event_ingested(&event.pubkey, event.kind)
                        .await;
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(event_id = %event.id, "insert failed: {e}");
                }
            }
        }

        let duration_ms = start.elapsed().as_millis() as u64;

        tracing::info!(
            relay = relay_url,
            kinds = ?kinds,
            authors = authors.len(),
            discovered = events_discovered,
            fetched = events_fetched,
            inserted = events_inserted,
            duration_ms = duration_ms,
            "per-author sync complete"
        );

        Ok(SyncStats {
            relay_url: relay_url.to_string(),
            events_discovered,
            events_fetched,
            events_inserted,
            duration_ms,
        })
    }

    /// Run negentropy sync using backward time windows (stateless, for test binary).
    pub async fn run_sync_windowed(
        &self,
        relay_url: &str,
        window_secs: Option<i64>,
        max_windows: usize,
    ) -> Result<SyncStats, AppError> {
        let now = chrono::Utc::now().timestamp();
        let initial = window_secs.unwrap_or(Self::DEFAULT_WINDOW_SECS);
        let (stats, _final_window, _final_cursor) = self
            .run_sync_windowed_from(relay_url, &[1], now, initial, max_windows)
            .await?;
        Ok(stats)
    }

    /// Earliest plausible Nostr timestamp (Jan 1 2020). Anything before this
    /// means we've walked past all possible Nostr data.
    pub const NOSTR_EPOCH: i64 = 1_577_836_800;

    /// Maximum window size for exponential growth (180 days).
    const MAX_WINDOW_SECS: i64 = 180 * 86400;

    /// Walk backward from a cursor in time windows for specific kinds.
    /// Uses exponential window growth: when a window finds 0 new events, double
    /// the window size (people go days/months without posting). When events are
    /// found, reset to the base window. Terminates only when cursor < NOSTR_EPOCH.
    ///
    /// Returns (SyncStats, final_window_secs, final_cursor) so callers can persist
    /// the window size for resumption across cycles.
    pub async fn run_sync_windowed_from(
        &self,
        relay_url: &str,
        kinds: &[i64],
        start_cursor: i64,
        initial_window_secs: i64,
        max_windows: usize,
    ) -> Result<(SyncStats, i64, i64), AppError> {
        let start = Instant::now();
        let mut window = initial_window_secs.max(Self::DEFAULT_WINDOW_SECS);
        let min_window: i64 = 3600;

        let mut total_discovered = 0usize;
        let mut total_fetched = 0usize;
        let mut total_inserted = 0usize;
        let mut cursor = start_cursor;
        let mut windows_done = 0usize;

        while windows_done < max_windows && cursor > Self::NOSTR_EPOCH {
            let window_since = (cursor - window).max(Self::NOSTR_EPOCH);
            let window_until = cursor;

            tracing::info!(
                relay = relay_url,
                kinds = ?kinds,
                window = windows_done + 1,
                since = window_since,
                until = window_until,
                window_days = window / 86400,
                window_hours = window / 3600,
                "starting sync window"
            );

            match self
                .run_sync_window(relay_url, kinds, Some(window_since), Some(window_until))
                .await
            {
                Ok(stats) => {
                    total_discovered += stats.events_discovered;
                    total_fetched += stats.events_fetched;
                    total_inserted += stats.events_inserted;
                    windows_done += 1;

                    if stats.events_discovered == 0 {
                        // Empty window: exponentially grow to skip quiet periods
                        let new_window = (window * 2).min(Self::MAX_WINDOW_SECS);
                        if new_window != window {
                            tracing::info!(
                                relay = relay_url,
                                kinds = ?kinds,
                                old_days = window / 86400,
                                new_days = new_window / 86400,
                                "empty window — doubling window size"
                            );
                        }
                        window = new_window;
                    } else {
                        // Found events: reset to base window for fine-grained crawling
                        window = Self::DEFAULT_WINDOW_SECS;
                    }

                    cursor = window_since;
                }
                Err(e) => {
                    let err_msg = format!("{e}");
                    if err_msg.contains("too many") || err_msg.contains("blocked") {
                        let new_window = window / 2;
                        if new_window < min_window {
                            tracing::warn!(
                                relay = relay_url,
                                window_secs = window,
                                "window too small, skipping this range"
                            );
                            cursor -= window;
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
                    } else {
                        tracing::warn!(
                            relay = relay_url,
                            error = %e,
                            "sync window failed, skipping"
                        );
                        cursor -= window;
                        windows_done += 1;
                    }
                }
            }
        }

        let duration_ms = start.elapsed().as_millis() as u64;

        tracing::info!(
            relay = relay_url,
            kinds = ?kinds,
            windows = windows_done,
            final_window_days = window / 86400,
            cursor_reached = cursor,
            total_discovered = total_discovered,
            total_fetched = total_fetched,
            total_inserted = total_inserted,
            duration_ms = duration_ms,
            "windowed sync complete"
        );

        Ok((SyncStats {
            relay_url: relay_url.to_string(),
            events_discovered: total_discovered,
            events_fetched: total_fetched,
            events_inserted: total_inserted,
            duration_ms,
        }, window, cursor))
    }

    // -----------------------------------------------------------------------
    // Exhaustive sync with persistent cursor
    // -----------------------------------------------------------------------

    /// Run an exhaustive sync cycle for a relay across all event kinds.
    /// Uses persistent cursors in `negentropy_sync_state` to resume from
    /// where it left off. Each cycle:
    ///   1. Recent pass: sync last 24h for all kinds (catch new content)
    ///   2. Backfill pass: for each non-fully-backfilled kind, walk backward
    ///      from the stored cursor using exponential window growth to skip
    ///      quiet periods efficiently
    pub async fn run_exhaustive_sync(
        &self,
        relay_url: &str,
        max_windows_per_kind: usize,
    ) -> Result<SyncStats, AppError> {
        let start = Instant::now();
        let now = chrono::Utc::now().timestamp();

        let mut total_discovered = 0usize;
        let mut total_fetched = 0usize;
        let mut total_inserted = 0usize;

        for &kind in Self::ALL_CRAWL_KINDS {
            let kinds = &[kind];

            // Load sync state for this relay+kind
            let state = self.get_sync_state(relay_url, kind).await?;

            // --- Recent pass: always sync last 24h ---
            let recent_since = now - Self::DEFAULT_WINDOW_SECS;
            match self
                .run_sync_window(relay_url, kinds, Some(recent_since), Some(now))
                .await
            {
                Ok(stats) => {
                    total_discovered += stats.events_discovered;
                    total_fetched += stats.events_fetched;
                    total_inserted += stats.events_inserted;

                    // Update newest_synced_at
                    if now > state.newest_synced_at {
                        self.update_sync_state(relay_url, kind, |s| {
                            s.newest_synced_at = now;
                        })
                        .await?;
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        relay = relay_url,
                        kind = kind,
                        error = %e,
                        "recent sync pass failed"
                    );
                }
            }

            // --- Backfill pass: walk backward from cursor ---
            if state.fully_backfilled {
                tracing::debug!(
                    relay = relay_url,
                    kind = kind,
                    "already fully backfilled, skipping"
                );
                continue;
            }

            // Determine starting cursor: use stored oldest_synced_at,
            // or start from now if this is the first time
            let backfill_cursor = if state.oldest_synced_at > 0 {
                state.oldest_synced_at
            } else {
                // First time: start from 24h ago (recent pass covers now-24h)
                recent_since
            };

            // Resume with the persisted window size (exponential state survives across cycles)
            let resume_window = state.current_window_secs.max(Self::DEFAULT_WINDOW_SECS);

            tracing::info!(
                relay = relay_url,
                kind = kind,
                cursor = backfill_cursor,
                cursor_date = %chrono::DateTime::from_timestamp(backfill_cursor, 0)
                    .map(|d| d.format("%Y-%m-%d").to_string())
                    .unwrap_or_default(),
                window_days = resume_window / 86400,
                "starting backfill pass"
            );

            match self
                .run_sync_windowed_from(
                    relay_url,
                    kinds,
                    backfill_cursor,
                    resume_window,
                    max_windows_per_kind,
                )
                .await
            {
                Ok((stats, final_window, final_cursor)) => {
                    total_discovered += stats.events_discovered;
                    total_fetched += stats.events_fetched;
                    total_inserted += stats.events_inserted;

                    let new_cursor = final_cursor.max(0);

                    // Only mark fully backfilled when we've reached pre-Nostr era
                    let fully_done = new_cursor <= Self::NOSTR_EPOCH;

                    self.update_sync_state(relay_url, kind, |s| {
                        s.oldest_synced_at = new_cursor;
                        s.total_discovered += stats.events_discovered as i64;
                        s.total_inserted += stats.events_inserted as i64;
                        s.current_window_secs = final_window;
                        // Reset consecutive_empty — exponential windows handle gaps now
                        s.consecutive_empty_windows = 0;
                        s.fully_backfilled = fully_done;
                    })
                    .await?;

                    if fully_done {
                        tracing::info!(
                            relay = relay_url,
                            kind = kind,
                            "marked as fully backfilled (reached pre-Nostr epoch)"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        relay = relay_url,
                        kind = kind,
                        error = %e,
                        "backfill pass failed"
                    );
                }
            }
        }

        let duration_ms = start.elapsed().as_millis() as u64;

        Ok(SyncStats {
            relay_url: relay_url.to_string(),
            events_discovered: total_discovered,
            events_fetched: total_fetched,
            events_inserted: total_inserted,
            duration_ms,
        })
    }

    // -----------------------------------------------------------------------
    // Sync state persistence
    // -----------------------------------------------------------------------

    pub async fn get_sync_state(
        &self,
        relay_url: &str,
        kind: i64,
    ) -> Result<SyncState, AppError> {
        let row = sqlx::query_as::<_, SyncStateRow>(
            "SELECT oldest_synced_at, newest_synced_at, fully_backfilled, total_discovered, total_inserted, consecutive_empty_windows, current_window_secs
             FROM negentropy_sync_state WHERE relay_url = $1 AND kind = $2",
        )
        .bind(relay_url)
        .bind(kind)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| r.into()).unwrap_or(SyncState {
            oldest_synced_at: 0,
            newest_synced_at: 0,
            fully_backfilled: false,
            total_discovered: 0,
            total_inserted: 0,
            consecutive_empty_windows: 0,
            current_window_secs: Self::DEFAULT_WINDOW_SECS,
        }))
    }

    pub async fn update_sync_state<F>(
        &self,
        relay_url: &str,
        kind: i64,
        f: F,
    ) -> Result<(), AppError>
    where
        F: FnOnce(&mut SyncState),
    {
        let mut state = self.get_sync_state(relay_url, kind).await?;
        f(&mut state);

        sqlx::query(
            "INSERT INTO negentropy_sync_state (relay_url, kind, oldest_synced_at, newest_synced_at, fully_backfilled, total_discovered, total_inserted, consecutive_empty_windows, current_window_secs, last_sync_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, NOW())
             ON CONFLICT (relay_url, kind) DO UPDATE SET
               oldest_synced_at = EXCLUDED.oldest_synced_at,
               newest_synced_at = EXCLUDED.newest_synced_at,
               fully_backfilled = EXCLUDED.fully_backfilled,
               total_discovered = EXCLUDED.total_discovered,
               total_inserted = EXCLUDED.total_inserted,
               consecutive_empty_windows = EXCLUDED.consecutive_empty_windows,
               current_window_secs = EXCLUDED.current_window_secs,
               last_sync_at = NOW()",
        )
        .bind(relay_url)
        .bind(kind)
        .bind(state.oldest_synced_at)
        .bind(state.newest_synced_at)
        .bind(state.fully_backfilled)
        .bind(state.total_discovered)
        .bind(state.total_inserted)
        .bind(state.consecutive_empty_windows)
        .bind(state.current_window_secs)
        .execute(&self.pool)
        .await?;

        Ok(())
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

#[derive(Debug, Clone)]
pub struct SyncState {
    pub oldest_synced_at: i64,
    pub newest_synced_at: i64,
    pub fully_backfilled: bool,
    pub total_discovered: i64,
    pub total_inserted: i64,
    pub consecutive_empty_windows: i32,
    pub current_window_secs: i64,
}

#[derive(sqlx::FromRow)]
struct SyncStateRow {
    oldest_synced_at: i64,
    newest_synced_at: i64,
    fully_backfilled: bool,
    total_discovered: i64,
    total_inserted: i64,
    consecutive_empty_windows: i32,
    current_window_secs: i64,
}

impl From<SyncStateRow> for SyncState {
    fn from(r: SyncStateRow) -> Self {
        Self {
            oldest_synced_at: r.oldest_synced_at,
            newest_synced_at: r.newest_synced_at,
            fully_backfilled: r.fully_backfilled,
            total_discovered: r.total_discovered,
            total_inserted: r.total_inserted,
            consecutive_empty_windows: r.consecutive_empty_windows,
            current_window_secs: r.current_window_secs,
        }
    }
}
