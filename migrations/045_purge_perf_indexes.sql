-- no-transaction
-- Indexes that make bulk purging tractable.
--
-- `follow_lists.event_id` is a FK to events(id) ON DELETE CASCADE with no
-- supporting index. Postgres runs RI cascade checks per deleted row, so a
-- 5000-row `DELETE FROM events` batch performs 5000 sequential scans of
-- follow_lists. Migration 036 fixed exactly this for follows.source_event_id
-- and documented the symptom; follow_lists was missed at the time.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_follow_lists_event_id
    ON follow_lists (event_id);

-- The purge deletes seen_events and zap_metadata rows by the ids of the events
-- being removed. zap_metadata.event_id is the PK, but these two are not indexed.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_seen_events_target_id
    ON seen_events (target_id);

CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_zap_metadata_zapped_event_id
    ON zap_metadata (zapped_event_id);

-- Purging by author needs these; both columns are otherwise unindexed.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_zap_metadata_sender
    ON zap_metadata (sender_pubkey);

CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_zap_metadata_recipient
    ON zap_metadata (recipient_pubkey);
