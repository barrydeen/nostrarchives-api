-- Materialized views for heavy analytics queries that were doing full table scans.
-- These should be refreshed periodically (every 30-60 min) instead of computed on every request.

-- 1. Client leaderboard: clients ranked by note count
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

-- 2. Relay leaderboard: relays ranked by user count from NIP-65 relay lists
-- Filter relay_url to max 255 chars to avoid btree index size limits.
CREATE MATERIALIZED VIEW IF NOT EXISTS mv_relay_leaderboard AS
WITH latest_relay_lists AS (
    SELECT DISTINCT ON (pubkey) pubkey, tags
    FROM events
    WHERE kind = 10002
    ORDER BY pubkey, created_at DESC
),
relay_urls AS (
    SELECT
        lrl.pubkey,
        LEFT(RTRIM(LOWER(tag ->> 1), '/'), 255) AS relay_url
    FROM latest_relay_lists lrl,
         jsonb_array_elements(lrl.tags::jsonb) AS tag
    WHERE tag ->> 0 = 'r'
      AND tag ->> 1 IS NOT NULL
      AND tag ->> 1 != ''
      AND LENGTH(tag ->> 1) <= 255
)
SELECT relay_url, COUNT(DISTINCT pubkey)::bigint AS user_count
FROM relay_urls
WHERE relay_url LIKE 'wss://%' OR relay_url LIKE 'ws://%'
GROUP BY relay_url
ORDER BY user_count DESC;

CREATE UNIQUE INDEX IF NOT EXISTS idx_mv_relay_leaderboard_url
    ON mv_relay_leaderboard (relay_url);

-- 3. New users: pubkeys first seen in events, pre-aggregated
-- We store the first_seen and event_count per pubkey so the new_users query
-- can just filter by first_seen >= now() - 24h without scanning the whole events table.
CREATE MATERIALIZED VIEW IF NOT EXISTS mv_pubkey_first_seen AS
SELECT pubkey,
       MIN(created_at) AS first_seen,
       COUNT(*)::bigint AS event_count
FROM events
GROUP BY pubkey;

CREATE UNIQUE INDEX IF NOT EXISTS idx_mv_pubkey_first_seen_pk
    ON mv_pubkey_first_seen (pubkey);
CREATE INDEX IF NOT EXISTS idx_mv_pubkey_first_seen_ts
    ON mv_pubkey_first_seen (first_seen DESC);
