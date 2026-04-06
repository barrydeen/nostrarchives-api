-- Replace mv_client_leaderboard with time-bucketed columns (today/7d/30d/all)
-- to support per-timeframe client leaderboard queries.
-- Follows the same conditional-aggregation pattern as mv_author_leaderboards.

DROP MATERIALIZED VIEW IF EXISTS mv_client_leaderboard;

CREATE MATERIALIZED VIEW mv_client_leaderboard AS
SELECT
    LOWER(tag_elem->>1) AS client_name,
    -- note counts per timeframe
    COUNT(DISTINCT e.id) FILTER (WHERE e.created_at >= EXTRACT(EPOCH FROM NOW())::bigint - 86400)::bigint       AS note_count_today,
    COUNT(DISTINCT e.id) FILTER (WHERE e.created_at >= EXTRACT(EPOCH FROM NOW())::bigint - 7  * 86400)::bigint  AS note_count_7d,
    COUNT(DISTINCT e.id) FILTER (WHERE e.created_at >= EXTRACT(EPOCH FROM NOW())::bigint - 30 * 86400)::bigint  AS note_count_30d,
    COUNT(DISTINCT e.id)::bigint                                                                                 AS note_count_all,
    -- user counts per timeframe
    COUNT(DISTINCT e.pubkey) FILTER (WHERE e.created_at >= EXTRACT(EPOCH FROM NOW())::bigint - 86400)::bigint       AS user_count_today,
    COUNT(DISTINCT e.pubkey) FILTER (WHERE e.created_at >= EXTRACT(EPOCH FROM NOW())::bigint - 7  * 86400)::bigint  AS user_count_7d,
    COUNT(DISTINCT e.pubkey) FILTER (WHERE e.created_at >= EXTRACT(EPOCH FROM NOW())::bigint - 30 * 86400)::bigint  AS user_count_30d,
    COUNT(DISTINCT e.pubkey)::bigint                                                                                 AS user_count_all
FROM events e
JOIN profile_search ps ON ps.pubkey = e.pubkey AND ps.follower_count >= 1,
     jsonb_array_elements(e.tags) AS tag_elem
WHERE tag_elem->>0 = 'client'
  AND e.kind = 1
  AND LENGTH(tag_elem->>1) BETWEEN 1 AND 100
  AND LOWER(tag_elem->>1) NOT IN ('mostr')
GROUP BY LOWER(tag_elem->>1)
HAVING COUNT(DISTINCT e.id) >= 2
ORDER BY note_count_all DESC;

CREATE UNIQUE INDEX IF NOT EXISTS idx_mv_client_leaderboard_name
    ON mv_client_leaderboard (client_name);

-- Partial indexes for per-range ORDER BY scans.
CREATE INDEX IF NOT EXISTS idx_mv_client_leaderboard_notes_today ON mv_client_leaderboard (note_count_today DESC);
CREATE INDEX IF NOT EXISTS idx_mv_client_leaderboard_notes_7d    ON mv_client_leaderboard (note_count_7d    DESC);
CREATE INDEX IF NOT EXISTS idx_mv_client_leaderboard_notes_30d   ON mv_client_leaderboard (note_count_30d   DESC);
CREATE INDEX IF NOT EXISTS idx_mv_client_leaderboard_notes_all   ON mv_client_leaderboard (note_count_all   DESC);
CREATE INDEX IF NOT EXISTS idx_mv_client_leaderboard_users_today ON mv_client_leaderboard (user_count_today DESC);
CREATE INDEX IF NOT EXISTS idx_mv_client_leaderboard_users_7d    ON mv_client_leaderboard (user_count_7d    DESC);
CREATE INDEX IF NOT EXISTS idx_mv_client_leaderboard_users_30d   ON mv_client_leaderboard (user_count_30d   DESC);
CREATE INDEX IF NOT EXISTS idx_mv_client_leaderboard_users_all   ON mv_client_leaderboard (user_count_all   DESC);
