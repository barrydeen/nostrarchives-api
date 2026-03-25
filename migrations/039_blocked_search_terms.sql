CREATE TABLE blocked_search_terms (
    term TEXT PRIMARY KEY,
    reason TEXT,
    blocked_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    blocked_by TEXT NOT NULL
);
