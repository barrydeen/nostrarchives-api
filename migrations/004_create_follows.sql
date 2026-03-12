-- Social graph tables: track the latest follow list per pubkey and the follower->followed edges.

CREATE TABLE IF NOT EXISTS follow_lists (
    pubkey      CHAR(64) PRIMARY KEY,
    event_id    CHAR(64) NOT NULL REFERENCES events (id) ON DELETE CASCADE,
    created_at  BIGINT   NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS follows (
    follower_pubkey CHAR(64) NOT NULL,
    followed_pubkey CHAR(64) NOT NULL,
    source_event_id CHAR(64) NOT NULL REFERENCES events (id) ON DELETE CASCADE,
    relay_hint      TEXT,
    created_at      BIGINT NOT NULL,

    PRIMARY KEY (follower_pubkey, followed_pubkey)
);

CREATE INDEX IF NOT EXISTS idx_follows_followed ON follows (followed_pubkey);
CREATE INDEX IF NOT EXISTS idx_follows_follower ON follows (follower_pubkey);
