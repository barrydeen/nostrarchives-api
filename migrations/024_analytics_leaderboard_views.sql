-- Materialized views for analytics leaderboard queries.
-- A single wide MV per domain covers all timeframes via conditional aggregation,
-- so one periodic refresh replaces many live full-table scans.

-- 1. Author leaderboards: note_count / like_count / repost_count per timeframe.
--    "today" = rolling 24 h, "7d" = 7 days, "30d" = 30 days, "all" = unbounded.
--    Only kind=1 events; created_at is a Unix timestamp (bigint).
CREATE MATERIALIZED VIEW IF NOT EXISTS mv_author_leaderboards AS
SELECT
    pubkey,
    -- note counts
    COUNT(*) FILTER (WHERE created_at >= EXTRACT(EPOCH FROM NOW())::bigint - 86400)::bigint       AS note_count_today,
    COUNT(*) FILTER (WHERE created_at >= EXTRACT(EPOCH FROM NOW())::bigint - 7  * 86400)::bigint  AS note_count_7d,
    COUNT(*) FILTER (WHERE created_at >= EXTRACT(EPOCH FROM NOW())::bigint - 30 * 86400)::bigint  AS note_count_30d,
    COUNT(*)::bigint                                                                               AS note_count_all,
    -- reaction (like) sums
    COALESCE(SUM(reaction_count) FILTER (WHERE created_at >= EXTRACT(EPOCH FROM NOW())::bigint - 86400),      0)::bigint AS like_count_today,
    COALESCE(SUM(reaction_count) FILTER (WHERE created_at >= EXTRACT(EPOCH FROM NOW())::bigint - 7  * 86400), 0)::bigint AS like_count_7d,
    COALESCE(SUM(reaction_count) FILTER (WHERE created_at >= EXTRACT(EPOCH FROM NOW())::bigint - 30 * 86400), 0)::bigint AS like_count_30d,
    COALESCE(SUM(reaction_count), 0)::bigint                                                                             AS like_count_all,
    -- repost sums
    COALESCE(SUM(repost_count) FILTER (WHERE created_at >= EXTRACT(EPOCH FROM NOW())::bigint - 86400),      0)::bigint AS repost_count_today,
    COALESCE(SUM(repost_count) FILTER (WHERE created_at >= EXTRACT(EPOCH FROM NOW())::bigint - 7  * 86400), 0)::bigint AS repost_count_7d,
    COALESCE(SUM(repost_count) FILTER (WHERE created_at >= EXTRACT(EPOCH FROM NOW())::bigint - 30 * 86400), 0)::bigint AS repost_count_30d,
    COALESCE(SUM(repost_count), 0)::bigint                                                                             AS repost_count_all
FROM events
WHERE kind = 1
GROUP BY pubkey;

CREATE UNIQUE INDEX IF NOT EXISTS idx_mv_author_leaderboards_pubkey
    ON mv_author_leaderboards (pubkey);

-- Partial indexes to speed up per-range ORDER BY scans.
CREATE INDEX IF NOT EXISTS idx_mv_author_leaderboards_note_today  ON mv_author_leaderboards (note_count_today  DESC);
CREATE INDEX IF NOT EXISTS idx_mv_author_leaderboards_note_7d     ON mv_author_leaderboards (note_count_7d     DESC);
CREATE INDEX IF NOT EXISTS idx_mv_author_leaderboards_note_30d    ON mv_author_leaderboards (note_count_30d    DESC);
CREATE INDEX IF NOT EXISTS idx_mv_author_leaderboards_note_all    ON mv_author_leaderboards (note_count_all    DESC);
CREATE INDEX IF NOT EXISTS idx_mv_author_leaderboards_like_today  ON mv_author_leaderboards (like_count_today  DESC);
CREATE INDEX IF NOT EXISTS idx_mv_author_leaderboards_like_7d     ON mv_author_leaderboards (like_count_7d     DESC);
CREATE INDEX IF NOT EXISTS idx_mv_author_leaderboards_like_30d    ON mv_author_leaderboards (like_count_30d    DESC);
CREATE INDEX IF NOT EXISTS idx_mv_author_leaderboards_like_all    ON mv_author_leaderboards (like_count_all    DESC);
CREATE INDEX IF NOT EXISTS idx_mv_author_leaderboards_repost_today ON mv_author_leaderboards (repost_count_today DESC);
CREATE INDEX IF NOT EXISTS idx_mv_author_leaderboards_repost_7d   ON mv_author_leaderboards (repost_count_7d   DESC);
CREATE INDEX IF NOT EXISTS idx_mv_author_leaderboards_repost_30d  ON mv_author_leaderboards (repost_count_30d  DESC);
CREATE INDEX IF NOT EXISTS idx_mv_author_leaderboards_repost_all  ON mv_author_leaderboards (repost_count_all  DESC);

-- 2. Zapper leaderboards: sats sent / received per timeframe.
--    UNION ALL produces one row per (pubkey, direction) pair.
CREATE MATERIALIZED VIEW IF NOT EXISTS mv_zapper_leaderboards AS
SELECT
    sender_pubkey AS pubkey,
    'sent'::text  AS direction,
    COALESCE((SUM(amount_msats) FILTER (WHERE created_at >= EXTRACT(EPOCH FROM NOW())::bigint - 86400)      / 1000), 0)::bigint AS sats_today,
    COUNT(*)      FILTER (WHERE created_at >= EXTRACT(EPOCH FROM NOW())::bigint - 86400)::bigint                               AS count_today,
    COALESCE((SUM(amount_msats) FILTER (WHERE created_at >= EXTRACT(EPOCH FROM NOW())::bigint - 7  * 86400) / 1000), 0)::bigint AS sats_7d,
    COUNT(*)      FILTER (WHERE created_at >= EXTRACT(EPOCH FROM NOW())::bigint - 7  * 86400)::bigint                           AS count_7d,
    COALESCE((SUM(amount_msats) FILTER (WHERE created_at >= EXTRACT(EPOCH FROM NOW())::bigint - 30 * 86400) / 1000), 0)::bigint AS sats_30d,
    COUNT(*)      FILTER (WHERE created_at >= EXTRACT(EPOCH FROM NOW())::bigint - 30 * 86400)::bigint                           AS count_30d,
    COALESCE((SUM(amount_msats) / 1000), 0)::bigint AS sats_all,
    COUNT(*)::bigint                                 AS count_all
FROM zap_metadata
WHERE sender_pubkey IS NOT NULL AND sender_pubkey != ''
GROUP BY sender_pubkey

UNION ALL

SELECT
    recipient_pubkey AS pubkey,
    'received'::text AS direction,
    COALESCE((SUM(amount_msats) FILTER (WHERE created_at >= EXTRACT(EPOCH FROM NOW())::bigint - 86400)      / 1000), 0)::bigint AS sats_today,
    COUNT(*)      FILTER (WHERE created_at >= EXTRACT(EPOCH FROM NOW())::bigint - 86400)::bigint                               AS count_today,
    COALESCE((SUM(amount_msats) FILTER (WHERE created_at >= EXTRACT(EPOCH FROM NOW())::bigint - 7  * 86400) / 1000), 0)::bigint AS sats_7d,
    COUNT(*)      FILTER (WHERE created_at >= EXTRACT(EPOCH FROM NOW())::bigint - 7  * 86400)::bigint                           AS count_7d,
    COALESCE((SUM(amount_msats) FILTER (WHERE created_at >= EXTRACT(EPOCH FROM NOW())::bigint - 30 * 86400) / 1000), 0)::bigint AS sats_30d,
    COUNT(*)      FILTER (WHERE created_at >= EXTRACT(EPOCH FROM NOW())::bigint - 30 * 86400)::bigint                           AS count_30d,
    COALESCE((SUM(amount_msats) / 1000), 0)::bigint AS sats_all,
    COUNT(*)::bigint                                 AS count_all
FROM zap_metadata
WHERE recipient_pubkey IS NOT NULL AND recipient_pubkey != ''
GROUP BY recipient_pubkey;

CREATE UNIQUE INDEX IF NOT EXISTS idx_mv_zapper_leaderboards_pk
    ON mv_zapper_leaderboards (pubkey, direction);

CREATE INDEX IF NOT EXISTS idx_mv_zapper_leaderboards_sats_today ON mv_zapper_leaderboards (direction, sats_today  DESC);
CREATE INDEX IF NOT EXISTS idx_mv_zapper_leaderboards_sats_7d    ON mv_zapper_leaderboards (direction, sats_7d     DESC);
CREATE INDEX IF NOT EXISTS idx_mv_zapper_leaderboards_sats_30d   ON mv_zapper_leaderboards (direction, sats_30d    DESC);
CREATE INDEX IF NOT EXISTS idx_mv_zapper_leaderboards_sats_all   ON mv_zapper_leaderboards (direction, sats_all    DESC);
