-- Enable trigram extension for fuzzy search
CREATE EXTENSION IF NOT EXISTS pg_trgm;

-- Helper: safely cast text to jsonb, returning empty object on failure
CREATE OR REPLACE FUNCTION safe_jsonb(input text) RETURNS jsonb
LANGUAGE plpgsql IMMUTABLE STRICT AS $$
BEGIN
    RETURN input::jsonb;
EXCEPTION WHEN OTHERS THEN
    RETURN '{}'::jsonb;
END;
$$;

-- Materialized view: latest profile metadata per pubkey with follower counts and engagement.
-- Provides pre-computed search surface for profile lookups.
CREATE MATERIALIZED VIEW profile_search AS
WITH latest_meta AS (
    SELECT DISTINCT ON (pubkey)
        pubkey,
        safe_jsonb(content) AS meta,
        created_at
    FROM events
    WHERE kind = 0
    ORDER BY pubkey, created_at DESC
),
followers AS (
    SELECT followed_pubkey AS pubkey, COUNT(*)::bigint AS follower_count
    FROM follows
    GROUP BY followed_pubkey
),
activity AS (
    SELECT pubkey, MAX(created_at)::bigint AS last_active_at
    FROM events
    GROUP BY pubkey
),
note_targets AS (
    SELECT id, pubkey
    FROM events
    WHERE kind = 1
),
engagement AS (
    SELECT
        nt.pubkey,
        COUNT(*) FILTER (WHERE r.ref_type = 'reaction')::bigint AS reactions,
        COUNT(*) FILTER (WHERE r.ref_type = 'reply')::bigint AS replies,
        COUNT(*) FILTER (WHERE r.ref_type = 'repost')::bigint AS reposts,
        COUNT(*) FILTER (WHERE r.ref_type = 'zap')::bigint AS zaps
    FROM event_refs r
    JOIN note_targets nt ON nt.id = r.target_event_id
    GROUP BY nt.pubkey
)
SELECT
    e.pubkey,
    NULLIF(TRIM(COALESCE(
        meta->>'display_name',
        meta->>'displayName'
    )), '') AS display_name,
    NULLIF(TRIM(COALESCE(
        meta->>'name',
        meta->>'username'
    )), '') AS name,
    NULLIF(TRIM(meta->>'nip05'), '') AS nip05,
    LEFT(NULLIF(TRIM(meta->>'about'), ''), 300) AS about,
    NULLIF(TRIM(COALESCE(meta->>'picture', meta->>'image')), '') AS picture,
    COALESCE(f.follower_count, 0)::bigint AS follower_count,
    COALESCE(a.last_active_at, e.created_at)::bigint AS last_active_at,
    COALESCE(eng.reactions, 0)::bigint AS reactions,
    COALESCE(eng.replies, 0)::bigint AS replies,
    COALESCE(eng.reposts, 0)::bigint AS reposts,
    COALESCE(eng.zaps, 0)::bigint AS zaps,
    (
        COALESCE(eng.reactions, 0)
        + COALESCE(eng.replies, 0) * 3
        + COALESCE(eng.reposts, 0) * 5
        + COALESCE(eng.zaps, 0) * 10
    )::bigint AS engagement_score,
    e.created_at AS metadata_created_at
FROM latest_meta e
LEFT JOIN followers f ON f.pubkey = e.pubkey
LEFT JOIN activity a ON a.pubkey = e.pubkey
LEFT JOIN engagement eng ON eng.pubkey = e.pubkey;

-- Unique index required for CONCURRENT refresh
CREATE UNIQUE INDEX idx_ps_pubkey ON profile_search (pubkey);

-- Trigram GIN indexes for fuzzy/prefix ILIKE matching
CREATE INDEX idx_ps_name_trgm ON profile_search USING gin (name gin_trgm_ops);
CREATE INDEX idx_ps_display_name_trgm ON profile_search USING gin (display_name gin_trgm_ops);
CREATE INDEX idx_ps_nip05_trgm ON profile_search USING gin (nip05 gin_trgm_ops);

-- Follower count / engagement for ranking sort
CREATE INDEX idx_ps_followers ON profile_search (follower_count DESC);
CREATE INDEX idx_ps_engagement ON profile_search (engagement_score DESC);
CREATE INDEX idx_ps_last_active ON profile_search (last_active_at DESC);
