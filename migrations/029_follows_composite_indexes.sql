-- Composite indexes on follows for fast sorted listing + count queries.
-- The old single-column indexes (idx_follows_followed, idx_follows_follower)
-- couldn't satisfy ORDER BY created_at DESC, causing slow sorts or wrong index picks.

CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_follows_followed_created
    ON follows (followed_pubkey, created_at DESC);

CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_follows_follower_created
    ON follows (follower_pubkey, created_at DESC);
