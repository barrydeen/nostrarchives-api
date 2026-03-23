-- Simplify profile_search MV: drop activity and engagement CTEs.
--
-- The engagement and activity CTEs scanned the entire 41 GB events table,
-- making REFRESH take 9+ minutes. These columns are only used by the
-- in-memory profile ranking cache, where follower_count is the dominant
-- signal anyway. Replace engagement_score with 0 and last_active_at with
-- the metadata created_at timestamp.

DROP MATERIALIZED VIEW IF EXISTS profile_search CASCADE;

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
    e.created_at::bigint AS last_active_at,
    0::bigint AS engagement_score,
    e.created_at AS metadata_created_at
FROM latest_meta e
LEFT JOIN followers f ON f.pubkey = e.pubkey;

-- Recreate indexes
CREATE UNIQUE INDEX idx_ps_pubkey ON profile_search (pubkey);
CREATE INDEX idx_ps_name_trgm ON profile_search USING gin (name gin_trgm_ops);
CREATE INDEX idx_ps_display_name_trgm ON profile_search USING gin (display_name gin_trgm_ops);
CREATE INDEX idx_ps_nip05_trgm ON profile_search USING gin (nip05 gin_trgm_ops);
CREATE INDEX idx_ps_followers ON profile_search (follower_count DESC);
CREATE INDEX idx_ps_engagement ON profile_search (engagement_score DESC);
CREATE INDEX idx_ps_last_active ON profile_search (last_active_at DESC);

-- Recreate mv_client_leaderboard (was dropped by CASCADE)
CREATE MATERIALIZED VIEW IF NOT EXISTS mv_client_leaderboard AS
SELECT LOWER(tag_elem->>1)                    AS client_name,
       COUNT(DISTINCT e.id)::bigint           AS note_count,
       COUNT(DISTINCT e.pubkey)::bigint       AS user_count
FROM events e
JOIN profile_search ps ON ps.pubkey = e.pubkey AND ps.follower_count >= 1,
     jsonb_array_elements(e.tags) AS tag_elem
WHERE tag_elem->>0 = 'client'
  AND e.kind = 1
  AND LENGTH(tag_elem->>1) BETWEEN 1 AND 100
  AND LOWER(tag_elem->>1) NOT IN ('mostr')
GROUP BY LOWER(tag_elem->>1)
HAVING COUNT(DISTINCT e.id) >= 2
ORDER BY note_count DESC;

CREATE UNIQUE INDEX IF NOT EXISTS idx_mv_client_leaderboard_name
    ON mv_client_leaderboard (client_name);
