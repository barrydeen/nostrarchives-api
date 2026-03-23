-- Lightweight search index table for full-text search.
--
-- The events table is 41 GB; bitmap heap scans for FTS touch thousands of
-- scattered pages. This table contains only the columns needed for ranking,
-- so it's ~20x smaller and fits in RAM. Queries rank against search_index,
-- then join back to events for the final N results.

CREATE TABLE IF NOT EXISTS search_index (
    event_id    TEXT PRIMARY KEY,
    pubkey      TEXT NOT NULL,
    created_at  BIGINT NOT NULL,
    content_tsv TSVECTOR,
    reaction_count  INTEGER NOT NULL DEFAULT 0,
    reply_count     INTEGER NOT NULL DEFAULT 0,
    repost_count    INTEGER NOT NULL DEFAULT 0,
    zap_count       INTEGER NOT NULL DEFAULT 0,
    zap_amount_msats BIGINT NOT NULL DEFAULT 0
);

-- GIN index for full-text search (replaces scanning idx_events_content_fts on the 41 GB table)
CREATE INDEX idx_search_index_content_fts ON search_index USING GIN (content_tsv);

-- For time-filtered queries and ordering
CREATE INDEX idx_search_index_created ON search_index (created_at DESC);

-- Engagement score index: enables index scan ordered by engagement, filtering by tsvector.
-- For common terms ("bitcoin", 10% match rate), Postgres scans ~2000 high-engagement rows
-- to find 200 matches instead of bitmap-scanning 800k+ rows.
CREATE INDEX idx_search_index_engagement ON search_index (
    (reaction_count * 100 + reply_count * 500 + repost_count * 1000 + zap_count * 2000) DESC
);

-- Populate from existing kind-1 events
INSERT INTO search_index (event_id, pubkey, created_at, content_tsv, reaction_count, reply_count, repost_count, zap_count, zap_amount_msats)
SELECT id, pubkey, created_at, content_tsv, reaction_count, reply_count, repost_count, zap_count, zap_amount_msats
FROM events
WHERE kind = 1
ON CONFLICT (event_id) DO NOTHING;

-- Trigger function: insert into search_index when a kind-1 event is inserted
CREATE OR REPLACE FUNCTION search_index_insert_trigger() RETURNS trigger AS $$
BEGIN
    IF NEW.kind = 1 THEN
        INSERT INTO search_index (event_id, pubkey, created_at, content_tsv, reaction_count, reply_count, repost_count, zap_count, zap_amount_msats)
        VALUES (NEW.id, NEW.pubkey, NEW.created_at, NEW.content_tsv, NEW.reaction_count, NEW.reply_count, NEW.repost_count, NEW.zap_count, NEW.zap_amount_msats)
        ON CONFLICT (event_id) DO NOTHING;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER trg_search_index_insert
    AFTER INSERT ON events
    FOR EACH ROW
    EXECUTE FUNCTION search_index_insert_trigger();

-- Trigger function: sync counter updates from events to search_index
CREATE OR REPLACE FUNCTION search_index_counter_sync_trigger() RETURNS trigger AS $$
BEGIN
    IF NEW.kind = 1 AND (
        NEW.reaction_count IS DISTINCT FROM OLD.reaction_count OR
        NEW.reply_count IS DISTINCT FROM OLD.reply_count OR
        NEW.repost_count IS DISTINCT FROM OLD.repost_count OR
        NEW.zap_count IS DISTINCT FROM OLD.zap_count OR
        NEW.zap_amount_msats IS DISTINCT FROM OLD.zap_amount_msats
    ) THEN
        UPDATE search_index SET
            reaction_count = NEW.reaction_count,
            reply_count = NEW.reply_count,
            repost_count = NEW.repost_count,
            zap_count = NEW.zap_count,
            zap_amount_msats = NEW.zap_amount_msats
        WHERE event_id = NEW.id;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER trg_search_index_counter_sync
    AFTER UPDATE ON events
    FOR EACH ROW
    EXECUTE FUNCTION search_index_counter_sync_trigger();
