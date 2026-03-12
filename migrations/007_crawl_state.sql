-- Crawl state: tracks per-author crawl progress for the intelligent crawler.
--
-- priority_tier: 1 = top authors (1000+ followers), 2 = 100+, 3 = 10+, 4 = rest
-- crawl_cursor: the oldest created_at we've fetched so far (works backward in time)
-- newest_seen_at: the newest event created_at we have for this author (for dedup)
-- next_crawl_at: when this author should next be crawled (backoff/scheduling)

CREATE TABLE IF NOT EXISTS crawl_state (
    pubkey          TEXT PRIMARY KEY,
    follower_count  BIGINT NOT NULL DEFAULT 0,
    priority_tier   SMALLINT NOT NULL DEFAULT 4,
    crawl_cursor    BIGINT,          -- oldest timestamp fetched (NULL = never crawled)
    newest_seen_at  BIGINT,          -- newest event timestamp we have
    notes_crawled   BIGINT NOT NULL DEFAULT 0,
    last_crawled_at TIMESTAMPTZ,
    next_crawl_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Index for the crawler's work queue: pick next authors to crawl
CREATE INDEX IF NOT EXISTS idx_crawl_state_queue
    ON crawl_state (priority_tier ASC, next_crawl_at ASC);

-- Index for bulk priority recalculation
CREATE INDEX IF NOT EXISTS idx_crawl_state_follower_count
    ON crawl_state (follower_count DESC);
