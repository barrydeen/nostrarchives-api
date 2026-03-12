-- Event references: links events together by relationship type.
-- No FK on target_event_id to handle out-of-order ingestion gracefully.
-- When a referenced event arrives later, existing refs already point to it.

CREATE TABLE IF NOT EXISTS event_refs (
    source_event_id  CHAR(64)  NOT NULL REFERENCES events (id) ON DELETE CASCADE,
    target_event_id  CHAR(64)  NOT NULL,
    ref_type         TEXT      NOT NULL,  -- 'reply', 'root', 'mention', 'reaction', 'repost', 'zap'
    relay_hint       TEXT,
    created_at       BIGINT    NOT NULL,  -- denormalized from source event for ordering

    PRIMARY KEY (source_event_id, target_event_id, ref_type)
);

-- "Give me all replies/reactions/zaps/reposts for event X" — the primary use case
CREATE INDEX idx_event_refs_target_type     ON event_refs (target_event_id, ref_type);

-- "Give me everything referencing event X, newest first"
CREATE INDEX idx_event_refs_target_created  ON event_refs (target_event_id, created_at DESC);

-- "What does event X reference?" (thread traversal)
CREATE INDEX idx_event_refs_source          ON event_refs (source_event_id);

-- Fast count queries per type
CREATE INDEX idx_event_refs_type            ON event_refs (ref_type);
