CREATE TABLE IF NOT EXISTS relay_capabilities (
    relay_url TEXT PRIMARY KEY,
    supports_negentropy BOOLEAN NOT NULL DEFAULT false,
    nip11_json JSONB,
    max_limit INTEGER,
    last_checked_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    check_interval_hours INTEGER NOT NULL DEFAULT 24
);

CREATE INDEX IF NOT EXISTS idx_relay_caps_negentropy ON relay_capabilities (supports_negentropy) WHERE supports_negentropy = true;
