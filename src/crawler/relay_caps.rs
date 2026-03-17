use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use negentropy::{Negentropy, NegentropyStorageVector};
use sqlx::PgPool;
use tokio::time::{timeout, Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::error::AppError;

/// Normalize a relay URL: lowercase, strip trailing slash, ensure wss:// prefix.
pub fn normalize_relay_url(url: &str) -> String {
    let mut u = url.trim().to_lowercase();
    // Strip trailing slash
    while u.ends_with('/') {
        u.pop();
    }
    // Ensure scheme
    if !u.starts_with("wss://") && !u.starts_with("ws://") {
        u = format!("wss://{u}");
    }
    u
}

/// Relay capabilities detected via NIP-11 and NEG-OPEN probing.
#[derive(Debug, Clone)]
pub struct RelayCaps {
    pub relay_url: String,
    pub supports_negentropy: bool,
    pub max_limit: Option<i32>,
    pub nip11: Option<serde_json::Value>,
    pub last_checked_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// NIP-11 probe
// ---------------------------------------------------------------------------

/// Probe a relay's NIP-11 document via HTTP.
/// Converts `wss://` → `https://` and `ws://` → `http://`.
pub async fn probe_nip11(relay_url: &str) -> Result<RelayCaps, AppError> {
    let http_url = relay_url
        .replacen("wss://", "https://", 1)
        .replacen("ws://", "http://", 1);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| AppError::Internal(format!("reqwest build: {e}")))?;

    let resp = client
        .get(&http_url)
        .header("Accept", "application/nostr+json")
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("NIP-11 fetch {relay_url}: {e}")))?;

    let nip11: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("NIP-11 parse {relay_url}: {e}")))?;

    let supports_negentropy = nip11
        .get("supported_nips")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().any(|n| n.as_i64() == Some(77) || n.as_u64() == Some(77)))
        .unwrap_or(false);

    let max_limit = nip11
        .pointer("/limitation/max_limit")
        .and_then(|v| v.as_i64())
        .map(|v| v as i32);

    Ok(RelayCaps {
        relay_url: relay_url.to_string(),
        supports_negentropy,
        max_limit,
        nip11: Some(nip11),
        last_checked_at: Utc::now(),
    })
}

// ---------------------------------------------------------------------------
// NEG-OPEN probe (fallback)
// ---------------------------------------------------------------------------

/// Probe negentropy support by sending a minimal NEG-OPEN and observing the response.
pub async fn probe_neg_open(relay_url: &str) -> Result<bool, AppError> {
    let (mut ws, _) = timeout(Duration::from_secs(5), connect_async(relay_url))
        .await
        .map_err(|_| AppError::Internal(format!("WS connect timeout: {relay_url}")))?
        .map_err(|e| AppError::Internal(format!("WS connect {relay_url}: {e}")))?;

    // Build a minimal negentropy message from empty storage
    let mut storage = NegentropyStorageVector::new();
    storage
        .seal()
        .map_err(|e| AppError::Internal(format!("neg seal: {e}")))?;
    let mut neg = Negentropy::owned(storage, 0)
        .map_err(|e| AppError::Internal(format!("neg new: {e}")))?;
    let init_msg = neg
        .initiate()
        .map_err(|e| AppError::Internal(format!("neg initiate: {e}")))?;

    let sub_id = "neg-probe-0";
    let filter = serde_json::json!({"kinds": [1], "limit": 1});
    let neg_open = serde_json::json!(["NEG-OPEN", sub_id, filter, hex::encode(&init_msg)]);

    ws.send(Message::Text(neg_open.to_string().into()))
        .await
        .map_err(|e| AppError::Internal(format!("WS send: {e}")))?;

    let result = timeout(Duration::from_secs(5), async {
        while let Some(msg) = ws.next().await {
            let msg = match msg {
                Ok(m) => m,
                Err(_) => return false,
            };
            if let Message::Text(txt) = msg {
                let txt_str: &str = txt.as_ref();
                if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(txt_str) {
                    if let Some(cmd) = arr.first().and_then(|v| v.as_str()) {
                        match cmd {
                            "NEG-MSG" => return true,
                            "NEG-ERR" | "NOTICE" => return false,
                            _ => continue,
                        }
                    }
                }
            }
        }
        false
    })
    .await
    .unwrap_or(false);

    // Best-effort close
    let neg_close = serde_json::json!(["NEG-CLOSE", sub_id]);
    let _ = ws.send(Message::Text(neg_close.to_string().into())).await;
    let _ = ws.close(None).await;

    Ok(result)
}

// ---------------------------------------------------------------------------
// DB persistence
// ---------------------------------------------------------------------------

/// Upsert relay capabilities into the database.
pub async fn upsert_relay_caps(pool: &PgPool, caps: &RelayCaps) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO relay_capabilities (relay_url, supports_negentropy, nip11_json, max_limit, last_checked_at)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (relay_url) DO UPDATE SET
           supports_negentropy = EXCLUDED.supports_negentropy,
           nip11_json = EXCLUDED.nip11_json,
           max_limit = EXCLUDED.max_limit,
           last_checked_at = EXCLUDED.last_checked_at",
    )
    .bind(&caps.relay_url)
    .bind(caps.supports_negentropy)
    .bind(&caps.nip11)
    .bind(caps.max_limit)
    .bind(caps.last_checked_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// Get relay capabilities from the database.
pub async fn get_relay_caps(
    pool: &PgPool,
    relay_url: &str,
) -> Result<Option<RelayCaps>, AppError> {
    let row = sqlx::query_as::<_, RelayCapsRow>(
        "SELECT relay_url, supports_negentropy, nip11_json, max_limit, last_checked_at
         FROM relay_capabilities WHERE relay_url = $1",
    )
    .bind(relay_url)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| r.into()))
}

/// Check DB for cached caps; if stale, re-probe and update.
pub async fn check_and_update_caps(
    pool: &PgPool,
    relay_url: &str,
) -> Result<RelayCaps, AppError> {
    // Check existing
    if let Some(existing) = get_relay_caps(pool, relay_url).await? {
        let stale_threshold = sqlx::query_scalar::<_, i32>(
            "SELECT check_interval_hours FROM relay_capabilities WHERE relay_url = $1",
        )
        .bind(relay_url)
        .fetch_optional(pool)
        .await?
        .unwrap_or(24);

        let age = Utc::now() - existing.last_checked_at;
        if age.num_hours() < stale_threshold as i64 {
            return Ok(existing);
        }
    }

    // Probe: NIP-11 first, NEG-OPEN fallback
    let mut caps = match probe_nip11(relay_url).await {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("NIP-11 probe failed for {relay_url}: {e}");
            RelayCaps {
                relay_url: relay_url.to_string(),
                supports_negentropy: false,
                max_limit: None,
                nip11: None,
                last_checked_at: Utc::now(),
            }
        }
    };

    if !caps.supports_negentropy {
        match probe_neg_open(relay_url).await {
            Ok(true) => {
                tracing::info!("{relay_url} supports negentropy (NEG-OPEN probe)");
                caps.supports_negentropy = true;
            }
            Ok(false) => {
                tracing::debug!("{relay_url} does not support negentropy");
            }
            Err(e) => {
                tracing::debug!("NEG-OPEN probe failed for {relay_url}: {e}");
            }
        }
    }

    upsert_relay_caps(pool, &caps).await?;
    Ok(caps)
}

/// Get all relay URLs that support negentropy.
pub async fn get_negentropy_relays(pool: &PgPool) -> Result<Vec<String>, AppError> {
    let urls = sqlx::query_scalar::<_, String>(
        "SELECT relay_url FROM relay_capabilities WHERE supports_negentropy = true",
    )
    .fetch_all(pool)
    .await?;
    Ok(urls)
}

/// Get all known relay capabilities.
pub async fn get_all_known_relays(pool: &PgPool) -> Result<Vec<RelayCaps>, AppError> {
    let rows = sqlx::query_as::<_, RelayCapsRow>(
        "SELECT relay_url, supports_negentropy, nip11_json, max_limit, last_checked_at
         FROM relay_capabilities ORDER BY relay_url",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| r.into()).collect())
}

// ---------------------------------------------------------------------------
// Relay discovery from NIP-65 (kind-10002) events
// ---------------------------------------------------------------------------

/// Discover relay URLs from kind-10002 events, returning (relay_url, user_count)
/// ordered by user_count descending.
pub async fn discover_relays_from_nip65(
    pool: &PgPool,
) -> Result<Vec<(String, i64)>, AppError> {
    let rows = sqlx::query_as::<_, RelayCount>(
        r#"
        SELECT relay_url, COUNT(DISTINCT pubkey) AS user_count
        FROM (
            SELECT pubkey,
                   RTRIM(LOWER(tag_arr->>1), '/') AS relay_url
            FROM events,
                 jsonb_array_elements(tags) AS tag_arr
            WHERE kind = 10002
              AND tag_arr->>0 = 'r'
              AND tag_arr->>1 IS NOT NULL
              AND tag_arr->>1 != ''
        ) sub
        WHERE relay_url LIKE 'wss://%' OR relay_url LIKE 'ws://%'
        GROUP BY relay_url
        ORDER BY user_count DESC
        "#,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(|r| (r.relay_url, r.user_count)).collect())
}

// ---------------------------------------------------------------------------
// Internal row types
// ---------------------------------------------------------------------------

#[derive(sqlx::FromRow)]
struct RelayCapsRow {
    relay_url: String,
    supports_negentropy: bool,
    nip11_json: Option<serde_json::Value>,
    max_limit: Option<i32>,
    last_checked_at: DateTime<Utc>,
}

impl From<RelayCapsRow> for RelayCaps {
    fn from(r: RelayCapsRow) -> Self {
        Self {
            relay_url: r.relay_url,
            supports_negentropy: r.supports_negentropy,
            max_limit: r.max_limit,
            nip11: r.nip11_json,
            last_checked_at: r.last_checked_at,
        }
    }
}

#[derive(sqlx::FromRow)]
struct RelayCount {
    relay_url: String,
    user_count: i64,
}
