-- v2: Add counter columns to events table for engagement tracking.
-- Instead of storing each reaction/repost as a full event, we increment counters.
ALTER TABLE events ADD COLUMN IF NOT EXISTS reaction_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE events ADD COLUMN IF NOT EXISTS repost_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE events ADD COLUMN IF NOT EXISTS reply_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE events ADD COLUMN IF NOT EXISTS zap_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE events ADD COLUMN IF NOT EXISTS zap_amount_msats BIGINT NOT NULL DEFAULT 0;

CREATE INDEX IF NOT EXISTS idx_events_reaction_count ON events (reaction_count DESC) WHERE kind = 1;
CREATE INDEX IF NOT EXISTS idx_events_repost_count ON events (repost_count DESC) WHERE kind = 1;
CREATE INDEX IF NOT EXISTS idx_events_reply_count ON events (reply_count DESC) WHERE kind = 1;
CREATE INDEX IF NOT EXISTS idx_events_zap_amount ON events (zap_amount_msats DESC) WHERE kind = 1;
