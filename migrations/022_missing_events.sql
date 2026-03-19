-- Queue for event IDs we know exist (referenced by stored zaps/reactions/reposts)
-- but don't have in our events table. A background task drains this using RelayFetcher.
--
-- priority: 2 = zap target (most important), 1 = reaction/repost target
-- attempt_count: give up after 5 failed fetch attempts

CREATE TABLE IF NOT EXISTS missing_events (
    event_id          TEXT PRIMARY KEY,
    relay_hint        TEXT,
    priority          SMALLINT NOT NULL DEFAULT 0,
    discovered_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_attempted_at TIMESTAMPTZ,
    attempt_count     SMALLINT NOT NULL DEFAULT 0,
    fetched           BOOLEAN NOT NULL DEFAULT false
);

CREATE INDEX IF NOT EXISTS idx_missing_events_queue
    ON missing_events (priority DESC, discovered_at ASC)
    WHERE fetched = false AND attempt_count < 5;

-- One-time seed: find all note IDs referenced by stored zap receipts that we don't have.
-- This backfills missing notes from the pre-fix era where zap targets were never queued.
INSERT INTO missing_events (event_id, relay_hint, priority)
SELECT DISTINCT
    tag ->> 1            AS event_id,
    NULLIF(tag ->> 2, '') AS relay_hint,
    2                    AS priority
FROM events,
     jsonb_array_elements(tags::jsonb) AS tag
WHERE kind = 9735
  AND tag ->> 0 = 'e'
  AND tag ->> 1 IS NOT NULL
  AND tag ->> 1 != ''
  AND length(tag ->> 1) = 64
  AND NOT EXISTS (
      SELECT 1 FROM events e2 WHERE e2.id = (tag ->> 1)
  )
ON CONFLICT (event_id) DO NOTHING;
