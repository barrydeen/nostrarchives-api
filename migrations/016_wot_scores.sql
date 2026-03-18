-- Web of Trust scores: two-level follower quality check.
CREATE TABLE IF NOT EXISTS wot_scores (
    pubkey            TEXT PRIMARY KEY,
    follower_count    INTEGER NOT NULL DEFAULT 0,
    quality_followers INTEGER NOT NULL DEFAULT 0,
    passes_wot        BOOLEAN NOT NULL DEFAULT false,
    computed_at       TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_wot_passes ON wot_scores (passes_wot) WHERE passes_wot = true;
