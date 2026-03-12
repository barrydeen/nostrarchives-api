-- Migration 008: Convert all char(64)/char(128) columns to text.
--
-- Problem: sqlx binds Rust &str as TEXT parameters. When the column type
-- is char(64), Postgres cannot use btree indexes for the comparison
-- (it falls back to sequential scans). This caused 3-10 second response
-- times on simple primary-key lookups.
--
-- Fix: Change all fixed-width character columns to text. Indexes remain
-- valid after the type change.

-- Drop materialized view that depends on events columns (will be recreated)
DROP MATERIALIZED VIEW IF EXISTS profile_search;

-- events
ALTER TABLE events ALTER COLUMN id TYPE text;
ALTER TABLE events ALTER COLUMN pubkey TYPE text;
ALTER TABLE events ALTER COLUMN sig TYPE text;

-- event_refs
ALTER TABLE event_refs ALTER COLUMN source_event_id TYPE text;
ALTER TABLE event_refs ALTER COLUMN target_event_id TYPE text;

-- event_tags
ALTER TABLE event_tags ALTER COLUMN event_id TYPE text;

-- follows
ALTER TABLE follows ALTER COLUMN follower_pubkey TYPE text;
ALTER TABLE follows ALTER COLUMN followed_pubkey TYPE text;
ALTER TABLE follows ALTER COLUMN source_event_id TYPE text;

-- follow_lists
ALTER TABLE follow_lists ALTER COLUMN pubkey TYPE text;
ALTER TABLE follow_lists ALTER COLUMN event_id TYPE text;

-- Recreate profile_search materialized view (identical definition)
CREATE MATERIALIZED VIEW profile_search AS
WITH latest_meta AS (
    SELECT DISTINCT ON (pubkey) pubkey, safe_jsonb(content) AS meta, created_at
    FROM events
    WHERE kind = 0
    ORDER BY pubkey, created_at DESC
),
followers AS (
    SELECT followed_pubkey AS pubkey, COUNT(*) AS follower_count
    FROM follows
    GROUP BY followed_pubkey
),
activity AS (
    SELECT pubkey, MAX(created_at) AS last_active_at
    FROM events
    GROUP BY pubkey
),
note_targets AS (
    SELECT id, pubkey FROM events WHERE kind = 1
),
engagement AS (
    SELECT
        nt.pubkey,
        COUNT(*) FILTER (WHERE r.ref_type = 'reaction') AS reactions,
        COUNT(*) FILTER (WHERE r.ref_type = 'reply')    AS replies,
        COUNT(*) FILTER (WHERE r.ref_type = 'repost')   AS reposts,
        COUNT(*) FILTER (WHERE r.ref_type = 'zap')      AS zaps
    FROM event_refs r
    JOIN note_targets nt ON nt.id = r.target_event_id
    GROUP BY nt.pubkey
)
SELECT
    e.pubkey,
    NULLIF(TRIM(COALESCE(e.meta->>'display_name', e.meta->>'displayName')), '') AS display_name,
    NULLIF(TRIM(COALESCE(e.meta->>'name', e.meta->>'username')), '')            AS name,
    NULLIF(TRIM(e.meta->>'nip05'), '')                                          AS nip05,
    LEFT(NULLIF(TRIM(e.meta->>'about'), ''), 300)                               AS about,
    NULLIF(TRIM(COALESCE(e.meta->>'picture', e.meta->>'image')), '')            AS picture,
    COALESCE(f.follower_count, 0)                                               AS follower_count,
    COALESCE(a.last_active_at, e.created_at)                                    AS last_active_at,
    COALESCE(eng.reactions, 0)                                                  AS reactions,
    COALESCE(eng.replies, 0)                                                    AS replies,
    COALESCE(eng.reposts, 0)                                                    AS reposts,
    COALESCE(eng.zaps, 0)                                                       AS zaps,
    COALESCE(eng.reactions, 0)
        + COALESCE(eng.replies, 0) * 3
        + COALESCE(eng.reposts, 0) * 5
        + COALESCE(eng.zaps, 0) * 10                                            AS engagement_score,
    e.created_at                                                                AS metadata_created_at
FROM latest_meta e
LEFT JOIN followers f   ON f.pubkey = e.pubkey
LEFT JOIN activity a    ON a.pubkey = e.pubkey
LEFT JOIN engagement eng ON eng.pubkey = e.pubkey;

-- Recreate indexes
CREATE UNIQUE INDEX idx_ps_pubkey            ON profile_search (pubkey);
CREATE INDEX idx_ps_name_trgm               ON profile_search USING gin (name gin_trgm_ops);
CREATE INDEX idx_ps_display_name_trgm       ON profile_search USING gin (display_name gin_trgm_ops);
CREATE INDEX idx_ps_nip05_trgm              ON profile_search USING gin (nip05 gin_trgm_ops);
CREATE INDEX idx_ps_followers               ON profile_search (follower_count DESC);
CREATE INDEX idx_ps_engagement              ON profile_search (engagement_score DESC);
CREATE INDEX idx_ps_last_active             ON profile_search (last_active_at DESC);
