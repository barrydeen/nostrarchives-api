-- Fix missing indexes causing full table scans on seen_events, zap_metadata, and follows.
--
-- seen_events: reapply_counters_for_event queries target_id+kind with no index (14.5B seq reads)
-- zap_metadata: reapply_counters_for_event queries zapped_event_id with no index (8B seq reads)
-- follows: trending_users queries created_at with no index (scans all 1.5M rows)

CREATE INDEX IF NOT EXISTS idx_seen_events_target_kind
    ON seen_events (target_id, kind);

CREATE INDEX IF NOT EXISTS idx_zap_metadata_zapped_event
    ON zap_metadata (zapped_event_id);

CREATE INDEX IF NOT EXISTS idx_follows_created_at
    ON follows (created_at DESC);
