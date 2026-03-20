-- GIN index on tags column, partial on kind=1 only.
-- Enables fast @> containment lookups for #t hashtag filters
-- instead of scanning all 6.6M rows with jsonb_array_elements.
-- Uses CONCURRENTLY to avoid blocking writes during the build.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_events_kind1_tags_gin
    ON events USING gin(tags)
    WHERE kind = 1;
