-- Flag kind:1 notes whose entire content is a bare JSON object/array as
-- machine/app payloads (e.g. game moves, bot data channels). These are not
-- human content and must not participate in trending/engagement rankings,
-- but remain fully stored and searchable.
--
-- Heuristic (near-zero false positives): content trimmed, starts with '{' or
-- '[', and parses as valid JSON resolving to an object or array. The lead char
-- + successful parse means prose merely *containing* JSON is untouched, and
-- markdown like [text](url) fails to parse.

ALTER TABLE events ADD COLUMN IF NOT EXISTS is_machine_note BOOLEAN NOT NULL DEFAULT false;

-- Backfill existing kind:1 rows. A session-local helper performs the same
-- check the Rust ingestion path uses, with exception handling so invalid JSON
-- is simply treated as non-machine (PG12+ compatible; avoids the PG16 IS JSON).
CREATE OR REPLACE FUNCTION pg_temp.is_machine_content(c text) RETURNS boolean AS $func$
DECLARE
    t text;
    v jsonb;
BEGIN
    IF c IS NULL THEN
        RETURN false;
    END IF;
    t := btrim(c, E' \t\n\r\f\v');
    IF t = '' OR left(t, 1) NOT IN ('{', '[') THEN
        RETURN false;
    END IF;
    BEGIN
        v := t::jsonb;
    EXCEPTION WHEN others THEN
        RETURN false;
    END;
    RETURN jsonb_typeof(v) IN ('object', 'array');
END;
$func$ LANGUAGE plpgsql;

UPDATE events
SET is_machine_note = true
WHERE kind = 1
  AND NOT is_machine_note
  AND left(btrim(content, E' \t\n\r\f\v'), 1) IN ('{', '[')
  AND pg_temp.is_machine_content(content);
