-- Track when zaps were last crawled for each author
ALTER TABLE crawl_state ADD COLUMN IF NOT EXISTS zaps_crawled_at TIMESTAMPTZ;
