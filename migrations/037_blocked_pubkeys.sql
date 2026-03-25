CREATE TABLE blocked_pubkeys (
    pubkey TEXT PRIMARY KEY,
    reason TEXT,
    blocked_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    blocked_by TEXT NOT NULL
);
