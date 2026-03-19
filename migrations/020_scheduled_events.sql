-- Scheduled events: future-dated Nostr events held for timed publication.
CREATE TABLE IF NOT EXISTS scheduled_events (
    id              TEXT PRIMARY KEY,          -- Nostr event id (sha256 hash)
    pubkey          TEXT NOT NULL,             -- Author pubkey (hex)
    kind            INTEGER NOT NULL,          -- Event kind
    created_at      BIGINT NOT NULL,           -- Scheduled timestamp (must be in the future at submission time)
    content         TEXT NOT NULL,             -- Event content
    tags            JSONB NOT NULL DEFAULT '[]',
    sig             TEXT NOT NULL,             -- Schnorr signature
    raw             JSONB NOT NULL,            -- Full raw event JSON for relay publishing
    status          TEXT NOT NULL DEFAULT 'pending',  -- pending | publishing | published | failed
    relays_sent     JSONB NOT NULL DEFAULT '[]',      -- Array of relay URLs successfully sent to
    relays_failed   JSONB NOT NULL DEFAULT '[]',      -- Array of relay URLs that failed
    submitted_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    published_at    TIMESTAMPTZ,
    error_message   TEXT
);

CREATE INDEX IF NOT EXISTS idx_scheduled_events_status_created
    ON scheduled_events (status, created_at)
    WHERE status = 'pending';

CREATE INDEX IF NOT EXISTS idx_scheduled_events_pubkey
    ON scheduled_events (pubkey, created_at DESC);
