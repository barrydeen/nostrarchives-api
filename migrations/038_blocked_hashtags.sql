CREATE TABLE blocked_hashtags (
    hashtag TEXT PRIMARY KEY,
    reason TEXT,
    blocked_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    blocked_by TEXT NOT NULL
);
