CREATE TABLE IF NOT EXISTS daily_analytics (
    date DATE PRIMARY KEY,
    active_users BIGINT NOT NULL DEFAULT 0,
    zaps_sent BIGINT NOT NULL DEFAULT 0,
    notes_posted BIGINT NOT NULL DEFAULT 0,
    computed_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
