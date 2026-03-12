use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A Nostr event as received from a relay (NIP-01).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NostrEvent {
    pub id: String,
    pub pubkey: String,
    pub created_at: i64,
    pub kind: i64,
    pub tags: Vec<Vec<String>>,
    pub content: String,
    pub sig: String,
}

/// A Nostr event as stored in PostgreSQL.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct StoredEvent {
    pub id: String,
    pub pubkey: String,
    pub created_at: i64,
    pub kind: i32,
    pub content: String,
    pub sig: String,
    pub tags: sqlx::types::Json<Vec<Vec<String>>>,
    pub raw: sqlx::types::Json<serde_json::Value>,
    pub relay_url: Option<String>,
    pub received_at: DateTime<Utc>,
}

/// Global statistics returned by the API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalStats {
    pub total_events: i64,
    pub unique_pubkeys: i64,
    pub events_by_kind: Vec<KindCount>,
    pub ingestion_rate_per_min: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct KindCount {
    pub kind: i32,
    pub count: i64,
}

/// Query parameters for the events endpoint.
#[derive(Debug, Deserialize)]
pub struct EventQuery {
    pub pubkey: Option<String>,
    pub kind: Option<i32>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub search: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// A reference between two events.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct EventRef {
    pub source_event_id: String,
    pub target_event_id: String,
    pub ref_type: String,
    pub relay_hint: Option<String>,
    pub created_at: i64,
}

/// Aggregated interaction counts for an event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventInteractions {
    pub replies: i64,
    pub reactions: i64,
    pub reposts: i64,
    pub zaps: i64,
    pub zap_sats: i64,
}

/// A trending note with composite engagement score.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrendingNote {
    pub event: StoredEvent,
    pub score: i64,
    pub zap_sats: i64,
    pub reposts: i64,
    pub replies: i64,
    pub reactions: i64,
}

/// A new user with their first-seen timestamp.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewUser {
    pub pubkey: String,
    pub first_seen: i64,
    pub event_count: i64,
}

/// A trending user by new follower gain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrendingUser {
    pub pubkey: String,
    pub new_followers: i64,
}

/// Daily network stats.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyStats {
    pub daily_active_users: i64,
    pub total_sats_sent: i64,
    pub daily_posts: i64,
}

/// Thread context: the event, its ancestors, and all interactions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventThread {
    pub event: StoredEvent,
    pub root_id: Option<String>,
    pub parent_id: Option<String>,
    pub interactions: EventInteractions,
    pub replies: Vec<StoredEvent>,
    pub reactions: Vec<StoredEvent>,
    pub reposts: Vec<StoredEvent>,
    pub zaps: Vec<StoredEvent>,
}

/// A profile search result with ranking metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileSearchResult {
    pub pubkey: String,
    pub name: Option<String>,
    pub display_name: Option<String>,
    pub nip05: Option<String>,
    pub about: Option<String>,
    pub picture: Option<String>,
    pub follower_count: i64,
    pub engagement_score: i64,
    pub last_active_at: i64,
    pub rank_score: f64,
}

/// A note search result with engagement metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoteSearchResult {
    pub event: StoredEvent,
    pub rank_score: f64,
    pub reactions: i64,
    pub replies: i64,
    pub reposts: i64,
    pub zaps: i64,
}
