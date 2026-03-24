-- Add index on follows.source_event_id to speed up CASCADE deletes.
--
-- The follows table has a foreign key: source_event_id → events(id) ON DELETE CASCADE.
-- Without an index on source_event_id, every DELETE from the events table
-- (e.g. replacing a kind-10002 relay list) triggers a sequential scan of the
-- entire follows table (~11M rows) to find cascading rows to delete.
-- This caused sustained high DB load (load average >5, 15%+ iowait).

CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_follows_source_event_id
    ON follows (source_event_id);
