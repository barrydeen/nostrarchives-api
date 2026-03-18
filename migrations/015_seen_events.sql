-- Lightweight dedup table for reactions/reposts.
-- Stores only the event ID, kind, target, and timestamp -- not the full event.
CREATE TABLE IF NOT EXISTS seen_events (
    event_id   TEXT PRIMARY KEY,
    kind       SMALLINT NOT NULL,
    target_id  TEXT NOT NULL,
    created_at BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_seen_events_created ON seen_events (created_at);
