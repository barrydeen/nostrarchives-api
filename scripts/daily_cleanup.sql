-- Daily cleanup: Remove data from authors who dropped below follower threshold
-- Run this as a cron job, e.g.: psql -d nostr_api -f daily_cleanup.sql

-- Configuration: minimum follower threshold (should match MIN_FOLLOWER_THRESHOLD env var)
-- Default: 5 followers minimum
\set threshold 5

-- Log what will be cleaned up
WITH cleanup_preview AS (
  SELECT 
    COUNT(DISTINCT e.id) as events_to_cleanup,
    COUNT(DISTINCT e.pubkey) as authors_to_cleanup
  FROM events e
  INNER JOIN profile_search ps ON ps.pubkey = e.pubkey
  WHERE ps.follower_count < :threshold
    AND e.kind NOT IN (0, 3) -- Keep profiles and follow lists
)
SELECT 
  'Daily cleanup preview (follower_count < ' || :threshold || '):' as info,
  events_to_cleanup,
  authors_to_cleanup
FROM cleanup_preview;

-- Only proceed if there's significant cleanup needed (>100 events)
-- This prevents excessive logging for small cleanups
DO $$ 
DECLARE
  cleanup_count INTEGER;
BEGIN
  SELECT COUNT(*)
  INTO cleanup_count
  FROM events e
  INNER JOIN profile_search ps ON ps.pubkey = e.pubkey
  WHERE ps.follower_count < 5 -- Use literal value here since variables don't work in this context
    AND e.kind NOT IN (0, 3);
    
  IF cleanup_count > 100 THEN
    RAISE NOTICE 'Daily cleanup: removing % events from authors with <5 followers', cleanup_count;
    
    -- Manual cleanup of zap_metadata (no FK)
    DELETE FROM zap_metadata 
    WHERE event_id IN (
      SELECT e.id 
      FROM events e
      INNER JOIN profile_search ps ON ps.pubkey = e.pubkey
      WHERE ps.follower_count < 5
        AND e.kind NOT IN (0, 3)
    );
    
    -- Delete events (will cascade to event_tags, event_refs)
    -- Keep profiles (kind 0) and follow lists (kind 3) to maintain social graph
    DELETE FROM events 
    WHERE id IN (
      SELECT e.id
      FROM events e
      INNER JOIN profile_search ps ON ps.pubkey = e.pubkey
      WHERE ps.follower_count < 5
        AND e.kind NOT IN (0, 3)
    );
    
    RAISE NOTICE 'Daily cleanup completed';
  ELSIF cleanup_count > 0 THEN
    RAISE NOTICE 'Daily cleanup: % events eligible but below threshold (100), skipping', cleanup_count;
  ELSE
    RAISE NOTICE 'Daily cleanup: no events to clean up';
  END IF;
END $$;