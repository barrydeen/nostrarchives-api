use std::fmt;
use std::time::{Duration, Instant};

use futures_util::{stream, SinkExt, StreamExt};
use serde_json::Value;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use crate::db::models::NostrEvent;
use crate::db::repository::EventRepository;
use crate::error::AppError;

const FOLLOW_LIST_KIND: i64 = 3;
const MESSAGE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_EVENTS_PER_RELAY: usize = 5000;
const MAX_CONCURRENCY: usize = 4;

#[derive(Debug, Default)]
pub struct SocialBootstrapStats {
    pub lists_seen: usize,
    pub updates_applied: usize,
}

#[derive(Debug)]
struct BootstrapError {
    message: String,
}

impl BootstrapError {
    fn new(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
        }
    }
}

impl fmt::Display for BootstrapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for BootstrapError {}

impl From<AppError> for BootstrapError {
    fn from(value: AppError) -> Self {
        BootstrapError::new(value.to_string())
    }
}

/// Best-effort bootstrap that queries each relay for follow lists (kind 3)
/// and hydrates the social graph before steady-state ingestion catches up.
pub async fn bootstrap_social_graph(repo: EventRepository, relay_urls: Vec<String>) {
    if relay_urls.is_empty() {
        return;
    }

    tracing::info!(relays = relay_urls.len(), "starting social graph bootstrap");

    let start = Instant::now();
    let mut total_lists = 0usize;
    let mut total_updates = 0usize;
    let mut completed = 0usize;

    let mut tasks = stream::iter(relay_urls.into_iter())
        .map(|relay| {
            let repo = repo.clone();
            async move {
                match fetch_follow_lists(&relay, repo).await {
                    Ok(stats) => Ok((relay, stats)),
                    Err(err) => Err((relay, err)),
                }
            }
        })
        .buffer_unordered(MAX_CONCURRENCY);

    while let Some(result) = tasks.next().await {
        match result {
            Ok((relay, stats)) => {
                completed += 1;
                total_lists += stats.lists_seen;
                total_updates += stats.updates_applied;
                tracing::info!(
                    relay = relay.as_str(),
                    follow_lists = stats.lists_seen,
                    updates = stats.updates_applied,
                    "social graph relay synced"
                );
            }
            Err((relay, err)) => {
                tracing::warn!(relay = relay.as_str(), error = %err, "social graph relay failed");
            }
        }
    }

    tracing::info!(
        relays = completed,
        follow_lists = total_lists,
        updates = total_updates,
        elapsed_secs = start.elapsed().as_secs_f32(),
        "social graph bootstrap finished"
    );
}

async fn fetch_follow_lists(
    relay_url: &str,
    repo: EventRepository,
) -> Result<SocialBootstrapStats, BootstrapError> {
    let (ws_stream, _) = tokio_tungstenite::connect_async(relay_url)
        .await
        .map_err(|e| BootstrapError::new(format!("connect failed: {e}")))?;
    let (mut write, mut read) = ws_stream.split();

    let sub_id = format!("social-bootstrap-{}", Uuid::new_v4().simple());
    let req = serde_json::json!([
        "REQ",
        &sub_id,
        {
            "kinds": [FOLLOW_LIST_KIND],
            "limit": MAX_EVENTS_PER_RELAY as i64,
        }
    ]);

    write
        .send(Message::Text(req.to_string().into()))
        .await
        .map_err(|e| BootstrapError::new(format!("failed to send REQ: {e}")))?;

    let mut stats = SocialBootstrapStats::default();

    loop {
        match timeout(MESSAGE_TIMEOUT, read.next()).await {
            Ok(Some(Ok(message))) => match message {
                Message::Text(payload) => {
                    if let Some(event) = parse_follow_list_event(&payload) {
                        stats.lists_seen += 1;
                        repo.insert_event(&event, relay_url).await?;
                        if repo.upsert_follow_list(&event).await?.is_some() {
                            stats.updates_applied += 1;
                        }
                        if stats.lists_seen >= MAX_EVENTS_PER_RELAY {
                            break;
                        }
                    }
                }
                Message::Ping(data) => {
                    let _ = write.send(Message::Pong(data)).await;
                }
                Message::Pong(_) => {}
                Message::Close(_) => break,
                Message::Binary(_) | Message::Frame(_) => {}
            },
            Ok(Some(Err(e))) => return Err(BootstrapError::new(format!("ws error: {e}"))),
            Ok(None) => break,
            Err(_) => break,
        }
    }

    Ok(stats)
}

fn parse_follow_list_event(text: &str) -> Option<NostrEvent> {
    let parsed: Value = serde_json::from_str(text).ok()?;
    let arr = parsed.as_array()?;

    if arr.len() < 3 {
        return None;
    }

    if arr.first().and_then(|v| v.as_str())? != "EVENT" {
        return None;
    }

    let event: NostrEvent = serde_json::from_value(arr[2].clone()).ok()?;
    if event.kind != FOLLOW_LIST_KIND {
        return None;
    }

    Some(event)
}
