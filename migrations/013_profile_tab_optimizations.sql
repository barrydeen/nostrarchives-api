-- Migration 013: Optimize profile tab queries with denormalized columns + zap_metadata table
--
-- Problems solved:
-- 1. profile_notes/replies: NOT EXISTS anti-join on event_refs → slow counts
-- 2. profile_zaps_sent: regex extraction of sender pubkey from JSON description
-- 3. profile_zaps_received: p-tag scan across all kinds

-- ─── is_reply column ────────────────────────────────────────────────

ALTER TABLE events ADD COLUMN IF NOT EXISTS is_reply BOOLEAN NOT NULL DEFAULT false;

-- Backfill: mark kind-1 events that have reply/root outgoing refs
UPDATE events e SET is_reply = true
FROM (SELECT DISTINCT source_event_id FROM event_refs WHERE ref_type IN ('reply', 'root')) r
WHERE e.id = r.source_event_id AND e.kind = 1 AND NOT e.is_reply;

-- Partial indexes for profile notes (non-replies) and replies
CREATE INDEX IF NOT EXISTS idx_events_profile_notes
    ON events (pubkey, created_at DESC) WHERE kind = 1 AND NOT is_reply;
CREATE INDEX IF NOT EXISTS idx_events_profile_replies
    ON events (pubkey, created_at DESC) WHERE kind = 1 AND is_reply;

-- ─── zap_metadata table ─────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS zap_metadata (
    event_id TEXT PRIMARY KEY,
    sender_pubkey TEXT,
    recipient_pubkey TEXT,
    amount_msats BIGINT NOT NULL DEFAULT 0,
    zapped_event_id TEXT,
    created_at BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_zap_metadata_sender
    ON zap_metadata (sender_pubkey, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_zap_metadata_recipient
    ON zap_metadata (recipient_pubkey, created_at DESC);

-- Backfill zap_metadata from existing kind-9735 events
INSERT INTO zap_metadata (event_id, sender_pubkey, recipient_pubkey, amount_msats, zapped_event_id, created_at)
SELECT
    e.id,
    LOWER(substring(td.tag_value from '"pubkey"\s*:\s*"([0-9a-fA-F]{64})"')),
    tp.tag_value,
    COALESCE(CASE WHEN ta.tag_value ~ '^[0-9]+$' THEN ta.tag_value::bigint ELSE 0 END, 0),
    te.tag_value,
    e.created_at
FROM events e
LEFT JOIN LATERAL (SELECT tag_value FROM event_tags WHERE event_id = e.id AND tag_name = 'description' LIMIT 1) td ON true
LEFT JOIN LATERAL (SELECT tag_value FROM event_tags WHERE event_id = e.id AND tag_name = 'p' LIMIT 1) tp ON true
LEFT JOIN LATERAL (SELECT tag_value FROM event_tags WHERE event_id = e.id AND tag_name = 'amount' LIMIT 1) ta ON true
LEFT JOIN LATERAL (SELECT tag_value FROM event_tags WHERE event_id = e.id AND tag_name = 'e' LIMIT 1) te ON true
WHERE e.kind = 9735
ON CONFLICT (event_id) DO NOTHING;
