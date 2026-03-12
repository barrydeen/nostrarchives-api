CREATE TABLE IF NOT EXISTS events (
    id          CHAR(64)     PRIMARY KEY,
    pubkey      CHAR(64)     NOT NULL,
    created_at  BIGINT       NOT NULL,
    kind        INTEGER      NOT NULL,
    content     TEXT         NOT NULL DEFAULT '',
    sig         CHAR(128)    NOT NULL,
    tags        JSONB        NOT NULL DEFAULT '[]',
    raw         JSONB        NOT NULL DEFAULT '{}',
    relay_url   TEXT,
    received_at TIMESTAMPTZ  NOT NULL DEFAULT NOW(),

    -- Generated full-text search column
    content_tsv TSVECTOR GENERATED ALWAYS AS (to_tsvector('english', content)) STORED
);

-- Core lookup indexes
CREATE INDEX idx_events_pubkey          ON events (pubkey);
CREATE INDEX idx_events_kind            ON events (kind);
CREATE INDEX idx_events_created_at      ON events (created_at DESC);

-- Composite indexes for common query patterns
CREATE INDEX idx_events_kind_created    ON events (kind, created_at DESC);
CREATE INDEX idx_events_pubkey_kind     ON events (pubkey, kind);
CREATE INDEX idx_events_pubkey_created  ON events (pubkey, created_at DESC);

-- Relay and ingestion tracking
CREATE INDEX idx_events_relay_url       ON events (relay_url);
CREATE INDEX idx_events_received_at     ON events (received_at DESC);

-- JSONB tags for flexible tag queries
CREATE INDEX idx_events_tags_gin        ON events USING GIN (tags);

-- Full-text search
CREATE INDEX idx_events_content_fts     ON events USING GIN (content_tsv);
