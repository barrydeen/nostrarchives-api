-- Covering index for trending notes query.
--
-- The trending notes query scans all kind=1 events in the last 24h to compute
-- an engagement score. Without a covering index, Postgres uses idx_events_kind_created
-- but must fetch every row from the heap to read engagement counters — causing ~46K
-- random disk reads and 36+ second query times.
--
-- This partial covering index:
-- 1. Only includes engaged events (partial WHERE), reducing index size from 59K to ~16K rows
-- 2. INCLUDEs all columns needed for score computation, enabling index-only scan
-- 3. Postgres scores and sorts using only the index, then fetches full rows for just the top N
--
-- Expected improvement: 36s → sub-second.

CREATE INDEX IF NOT EXISTS idx_events_trending_candidates
    ON events (created_at DESC)
    INCLUDE (id, pubkey, reaction_count, repost_count, reply_count, zap_count, zap_amount_msats)
    WHERE kind = 1
      AND (reaction_count + repost_count + reply_count + zap_count) > 0;
