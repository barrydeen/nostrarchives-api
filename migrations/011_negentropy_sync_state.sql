CREATE TABLE IF NOT EXISTS negentropy_sync_state (
    relay_url TEXT NOT NULL,
    kind BIGINT NOT NULL,
    oldest_synced_at BIGINT NOT NULL DEFAULT 0,
    newest_synced_at BIGINT NOT NULL DEFAULT 0,
    fully_backfilled BOOLEAN NOT NULL DEFAULT false,
    total_discovered BIGINT NOT NULL DEFAULT 0,
    total_inserted BIGINT NOT NULL DEFAULT 0,
    last_sync_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    consecutive_empty_windows INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (relay_url, kind)
);

CREATE INDEX IF NOT EXISTS idx_neg_sync_backfill ON negentropy_sync_state (fully_backfilled) WHERE fully_backfilled = false;
