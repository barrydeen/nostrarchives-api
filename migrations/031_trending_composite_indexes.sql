-- Composite indexes for trending/top-notes queries by time range + engagement.
--
-- The top_notes_unified query needs: WHERE kind=1 AND created_at >= X AND {counter} > 0
-- ORDER BY {counter} DESC. Without a composite index, Postgres either:
-- (a) scans idx_events_reaction_count (all-time, pre-sorted) and filters by time — skips
--     30K+ old rows with random heap reads (25s for "today"), or
-- (b) scans idx_events_kind_created (time-filtered) and sorts 59K rows by counter.
--
-- These partial indexes cover both dimensions: time filter + engagement sort.
-- Postgres can scan only today's engaged notes, already sorted by the metric.

CREATE INDEX IF NOT EXISTS idx_events_k1_created_reactions
    ON events (created_at DESC, reaction_count DESC)
    WHERE kind = 1 AND reaction_count > 0;

CREATE INDEX IF NOT EXISTS idx_events_k1_created_replies
    ON events (created_at DESC, reply_count DESC)
    WHERE kind = 1 AND reply_count > 0;

CREATE INDEX IF NOT EXISTS idx_events_k1_created_reposts
    ON events (created_at DESC, repost_count DESC)
    WHERE kind = 1 AND repost_count > 0;

CREATE INDEX IF NOT EXISTS idx_events_k1_created_zaps
    ON events (created_at DESC, zap_amount_msats DESC)
    WHERE kind = 1 AND zap_amount_msats > 0;
