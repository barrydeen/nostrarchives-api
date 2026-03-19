use std::sync::Arc;
use std::time::{Duration, Instant};

use sqlx::{PgPool, Row};
use tokio::sync::RwLock;

use crate::error::AppError;

/// A single cached profile with pre-lowercased search fields.
#[derive(Clone)]
struct CachedProfile {
    pubkey: String,
    name: Option<String>,
    display_name: Option<String>,
    nip05: Option<String>,
    about: Option<String>,
    picture: Option<String>,
    follower_count: i64,
    engagement_score: i64,
    last_active_at: i64,
    // Pre-lowercased for search (avoids per-query allocation)
    name_lower: String,
    display_name_lower: String,
    nip05_lower: String,
}

/// In-memory profile search index.
///
/// Loads all profiles from `profile_search` into memory, sorted by
/// follower_count DESC.  Searches scan the Vec and apply the same ranking
/// formula as the SQL version, but without any database round-trip.
///
/// Refresh interval is configurable (default 24 h).  The materialized view
/// still refreshes every 5 min in the background — that's fine; we just
/// snapshot it into RAM less often.
#[derive(Clone)]
pub struct ProfileSearchCache {
    profiles: Arc<RwLock<Vec<CachedProfile>>>,
    last_refresh: Arc<RwLock<Instant>>,
    refresh_interval: Duration,
    pool: PgPool,
}

impl ProfileSearchCache {
    pub fn new(pool: PgPool, refresh_interval_secs: u64) -> Self {
        Self {
            profiles: Arc::new(RwLock::new(Vec::new())),
            last_refresh: Arc::new(RwLock::new(
                Instant::now() - Duration::from_secs(refresh_interval_secs + 1),
            )),
            refresh_interval: Duration::from_secs(refresh_interval_secs),
            pool,
        }
    }

    /// Load profiles from DB into memory.  Called on startup and periodically.
    pub async fn initialize(&self) -> Result<(), AppError> {
        self.refresh().await
    }

    /// Start background refresh loop.
    pub fn spawn_refresh_loop(self, interval: Duration) {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                ticker.tick().await;
                if let Err(e) = self.refresh().await {
                    tracing::warn!(error = %e, "profile search cache refresh failed");
                }
            }
        });
    }

    /// Check staleness and refresh if needed (lazy path — called from handlers).
    async fn ensure_fresh(&self) {
        let stale = {
            let ts = self.last_refresh.read().await;
            ts.elapsed() > self.refresh_interval
        };
        if stale {
            if let Err(e) = self.refresh().await {
                tracing::warn!(error = %e, "lazy profile search cache refresh failed");
            }
        }
    }

    async fn refresh(&self) -> Result<(), AppError> {
        let start = Instant::now();
        tracing::info!("refreshing profile search cache from DB…");

        let rows = sqlx::query(
            r#"
            SELECT pubkey, name, display_name, nip05, about, picture,
                   follower_count, engagement_score, last_active_at
            FROM profile_search
            ORDER BY follower_count DESC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        let mut profiles = Vec::with_capacity(rows.len());
        for row in &rows {
            let name: Option<String> = row.try_get("name").ok().flatten();
            let display_name: Option<String> = row.try_get("display_name").ok().flatten();
            let nip05: Option<String> = row.try_get("nip05").ok().flatten();

            profiles.push(CachedProfile {
                pubkey: row.try_get("pubkey").unwrap_or_default(),
                name_lower: name.as_deref().unwrap_or("").to_lowercase(),
                display_name_lower: display_name.as_deref().unwrap_or("").to_lowercase(),
                nip05_lower: nip05.as_deref().unwrap_or("").to_lowercase(),
                name,
                display_name,
                nip05,
                about: row.try_get("about").ok().flatten(),
                picture: row.try_get("picture").ok().flatten(),
                follower_count: row.try_get("follower_count").unwrap_or(0),
                engagement_score: row.try_get("engagement_score").unwrap_or(0),
                last_active_at: row.try_get("last_active_at").unwrap_or(0),
            });
        }

        let count = profiles.len();
        {
            let mut cache = self.profiles.write().await;
            *cache = profiles;
        }
        {
            let mut ts = self.last_refresh.write().await;
            *ts = Instant::now();
        }

        let elapsed = start.elapsed();
        tracing::info!(
            count,
            elapsed_ms = elapsed.as_millis(),
            "profile search cache loaded"
        );
        Ok(())
    }

    // ── public search APIs ─────────────────────────────────────────────

    /// Full profile search (mirrors `search_profiles` SQL).
    pub async fn search_profiles(
        &self,
        query: &str,
        limit: i64,
        offset: i64,
    ) -> Vec<crate::db::models::ProfileSearchResult> {
        self.ensure_fresh().await;
        let q = query.to_lowercase();
        let now_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let profiles = self.profiles.read().await;

        // Collect matching profiles with scores
        let mut scored: Vec<(usize, f64)> = Vec::new();

        for (idx, p) in profiles.iter().enumerate() {
            // Substring match on name, display_name, nip05
            let matches = p.name_lower.contains(&q)
                || p.display_name_lower.contains(&q)
                || p.nip05_lower.contains(&q);

            if !matches {
                continue;
            }

            let score = rank_score(p, &q, now_epoch);
            scored.push((idx, score));
        }

        // Sort by score DESC, then follower_count DESC
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    profiles[b.0]
                        .follower_count
                        .cmp(&profiles[a.0].follower_count)
                })
        });

        // Apply offset + limit
        scored
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .map(|(idx, score)| {
                let p = &profiles[idx];
                crate::db::models::ProfileSearchResult {
                    pubkey: p.pubkey.clone(),
                    name: p.name.clone(),
                    display_name: p.display_name.clone(),
                    nip05: p.nip05.clone(),
                    about: p.about.clone(),
                    picture: p.picture.clone(),
                    follower_count: p.follower_count,
                    engagement_score: p.engagement_score,
                    last_active_at: p.last_active_at,
                    rank_score: score,
                }
            })
            .collect()
    }

    /// Lightweight autocomplete (mirrors `suggest_profiles` SQL).
    pub async fn suggest_profiles(
        &self,
        query: &str,
        limit: i64,
    ) -> Vec<crate::db::models::ProfileSearchResult> {
        self.ensure_fresh().await;
        let q = query.to_lowercase();

        let profiles = self.profiles.read().await;

        let mut scored: Vec<(usize, f64)> = Vec::new();

        for (idx, p) in profiles.iter().enumerate() {
            // Match: substring on name/display_name/nip05 OR prefix on pubkey
            let matches = p.name_lower.contains(&q)
                || p.display_name_lower.contains(&q)
                || p.nip05_lower.contains(&q)
                || p.pubkey.starts_with(&q);

            if !matches {
                continue;
            }

            let score = suggest_score(p, &q);
            scored.push((idx, score));
        }

        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    profiles[b.0]
                        .follower_count
                        .cmp(&profiles[a.0].follower_count)
                })
        });

        scored
            .into_iter()
            .take(limit as usize)
            .map(|(idx, score)| {
                let p = &profiles[idx];
                crate::db::models::ProfileSearchResult {
                    pubkey: p.pubkey.clone(),
                    name: p.name.clone(),
                    display_name: p.display_name.clone(),
                    nip05: p.nip05.clone(),
                    about: None, // autocomplete doesn't need about
                    picture: p.picture.clone(),
                    follower_count: p.follower_count,
                    engagement_score: p.engagement_score,
                    last_active_at: p.last_active_at,
                    rank_score: score,
                }
            })
            .collect()
    }

    /// Cache statistics for monitoring.
    pub async fn stats(&self) -> ProfileSearchCacheStats {
        let count = self.profiles.read().await.len();
        let last_refresh_ago = self.last_refresh.read().await.elapsed();
        ProfileSearchCacheStats {
            profile_count: count,
            last_refresh_ago,
            refresh_interval: self.refresh_interval,
        }
    }
}

// ── ranking functions (pure, no DB) ────────────────────────────────────

/// Full search ranking — mirrors the SQL CASE expression.
fn rank_score(p: &CachedProfile, q: &str, now_epoch: i64) -> f64 {
    let mut score: f64 = 0.0;

    // Exact match bonuses
    if p.name_lower == q {
        score += 500.0;
    }
    if p.display_name_lower == q {
        score += 500.0;
    }
    if p.nip05_lower == q {
        score += 400.0;
    }

    // Prefix match bonuses
    if p.name_lower.starts_with(q) {
        score += 200.0;
    }
    if p.display_name_lower.starts_with(q) {
        score += 200.0;
    }
    if p.nip05_lower.starts_with(q) {
        score += 100.0;
    }

    // Trigram-like similarity (simplified: Jaccard on character bigrams)
    let sim = f64::max(
        bigram_similarity(&p.name_lower, q),
        bigram_similarity(&p.display_name_lower, q),
    );
    score += sim * 100.0;

    // Follower / engagement / recency
    score += (p.follower_count.max(0) as f64 + 1.0).ln() * 100.0;
    score += (p.engagement_score.max(0) as f64 + 1.0).ln() * 50.0;

    let age = now_epoch - p.last_active_at;
    if age < 604_800 {
        score += 200.0;
    } else if age < 2_592_000 {
        score += 100.0;
    }

    score
}

/// Suggest ranking — lighter weight.
fn suggest_score(p: &CachedProfile, q: &str) -> f64 {
    let mut score: f64 = 0.0;

    if p.name_lower == q || p.display_name_lower == q {
        score += 500.0;
    } else if p.name_lower.starts_with(q) || p.display_name_lower.starts_with(q) {
        score += 200.0;
    } else if p.nip05_lower.starts_with(q) {
        score += 100.0;
    }

    score += (p.follower_count.max(0) as f64 + 1.0).ln() * 100.0;
    score += (p.engagement_score.max(0) as f64 + 1.0).ln() * 50.0;

    score
}

/// Character-bigram Jaccard similarity (cheap approximation of pg_trgm).
fn bigram_similarity(a: &str, b: &str) -> f64 {
    if a.is_empty() || b.is_empty() || a.len() < 2 || b.len() < 2 {
        return 0.0;
    }

    let bigrams_a: std::collections::HashSet<(char, char)> =
        a.chars().zip(a.chars().skip(1)).collect();
    let bigrams_b: std::collections::HashSet<(char, char)> =
        b.chars().zip(b.chars().skip(1)).collect();

    let intersection = bigrams_a.intersection(&bigrams_b).count() as f64;
    let union = bigrams_a.union(&bigrams_b).count() as f64;

    if union == 0.0 {
        0.0
    } else {
        intersection / union
    }
}

#[derive(Debug)]
pub struct ProfileSearchCacheStats {
    pub profile_count: usize,
    pub last_refresh_ago: Duration,
    pub refresh_interval: Duration,
}
