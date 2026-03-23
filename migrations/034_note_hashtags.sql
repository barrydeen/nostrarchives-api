-- Lightweight hashtag lookup table for fast hashtag search.
--
-- Previously hashtag search used GIN containment (tags @> '[["t","tag"]]') on
-- the 41 GB events table, causing slow bitmap heap scans. This table stores
-- only t-tag data with a btree index for instant lookups.
--
-- Much smaller than the old event_tags table because it only stores t-tags,
-- not all tag types.

CREATE TABLE IF NOT EXISTS note_hashtags (
    event_id   TEXT NOT NULL,
    hashtag    TEXT NOT NULL,
    created_at BIGINT NOT NULL
);

-- Primary lookup: find notes by hashtag, ordered by recency
CREATE INDEX idx_note_hashtags_lookup ON note_hashtags (hashtag, created_at DESC);

-- Reverse lookup: find hashtags for a given event (for deletion/maintenance)
CREATE INDEX idx_note_hashtags_event ON note_hashtags (event_id);

-- Populate from existing kind-1 events with t-tags.
-- Extract hashtags from the jsonb tags array where tag[0] = 't'.
INSERT INTO note_hashtags (event_id, hashtag, created_at)
SELECT DISTINCT e.id, LOWER(tag_elem->>1), e.created_at
FROM events e,
     jsonb_array_elements(e.tags) AS tag_elem
WHERE e.kind = 1
  AND tag_elem->>0 = 't'
  AND LENGTH(tag_elem->>1) BETWEEN 1 AND 100;

-- Trigger function: insert hashtags when a kind-1 event is inserted
CREATE OR REPLACE FUNCTION note_hashtags_insert_trigger() RETURNS trigger AS $$
DECLARE
    tag_elem jsonb;
    tag_value text;
BEGIN
    IF NEW.kind = 1 AND NEW.tags IS NOT NULL THEN
        FOR tag_elem IN SELECT jsonb_array_elements(NEW.tags)
        LOOP
            IF tag_elem->>0 = 't' THEN
                tag_value := LOWER(tag_elem->>1);
                IF tag_value IS NOT NULL AND LENGTH(tag_value) BETWEEN 1 AND 100 THEN
                    INSERT INTO note_hashtags (event_id, hashtag, created_at)
                    VALUES (NEW.id, tag_value, NEW.created_at)
                    ON CONFLICT DO NOTHING;
                END IF;
            END IF;
        END LOOP;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER trg_note_hashtags_insert
    AFTER INSERT ON events
    FOR EACH ROW
    EXECUTE FUNCTION note_hashtags_insert_trigger();
