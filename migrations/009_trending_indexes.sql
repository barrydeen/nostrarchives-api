-- Migration 009: Add composite index for time-filtered trending queries.
--
-- The unified trending endpoint filters event_refs by ref_type + created_at.
-- This composite index allows efficient index scans instead of seq scans
-- on large time ranges.

CREATE INDEX IF NOT EXISTS idx_event_refs_reftype_created
    ON event_refs (ref_type, created_at DESC);
