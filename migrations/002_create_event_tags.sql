CREATE TABLE IF NOT EXISTS event_tags (
    event_id     CHAR(64)  NOT NULL REFERENCES events (id) ON DELETE CASCADE,
    tag_name     TEXT       NOT NULL,
    tag_value    TEXT       NOT NULL,
    extra_values JSONB      DEFAULT '[]',
    PRIMARY KEY (event_id, tag_name, tag_value)
);

-- Fast lookups: "find all events with tag p = <pubkey>" etc.
CREATE INDEX idx_event_tags_name_value  ON event_tags (tag_name, tag_value);
CREATE INDEX idx_event_tags_name        ON event_tags (tag_name);
