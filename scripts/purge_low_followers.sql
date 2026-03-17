-- One-time purge of events from authors with <5 followers
-- This script safely removes data with foreign key cascades

-- Summary before deletion
SELECT 
  'Before purge:' as phase,
  (SELECT COUNT(*) FROM events) as total_events,
  (SELECT COUNT(DISTINCT pubkey) FROM events) as total_authors,
  (SELECT pg_size_pretty(pg_database_size('nostr_api'))) as db_size;

-- Count what will be purged
WITH low_follower_authors AS (
  SELECT DISTINCT e.pubkey
  FROM events e
  LEFT JOIN profile_search ps ON ps.pubkey = e.pubkey
  WHERE COALESCE(ps.follower_count, 0) < 5
),
purge_stats AS (
  SELECT 
    COUNT(DISTINCT e.id) as events_to_purge,
    COUNT(DISTINCT e.pubkey) as authors_to_purge,
    (SELECT COUNT(*) FROM events) as total_events,
    (SELECT COUNT(DISTINCT pubkey) FROM events) as total_authors
  FROM events e 
  INNER JOIN low_follower_authors lfa ON lfa.pubkey = e.pubkey
)
SELECT 
  'Purge preview:' as phase,
  events_to_purge,
  authors_to_purge, 
  total_events,
  total_authors,
  ROUND(100.0 * events_to_purge / total_events, 1) as pct_events_purged,
  ROUND(100.0 * authors_to_purge / total_authors, 1) as pct_authors_purged
FROM purge_stats;

-- THE ACTUAL PURGE (uncomment to execute)
-- WARNING: This will permanently delete data
/*
-- Delete events from authors with <5 followers
-- Foreign key cascades will automatically clean up:
-- - event_tags (CASCADE)  
-- - event_refs (CASCADE)
-- - follows (CASCADE)
-- - follow_lists (CASCADE) 
-- - zap_metadata (no FK, needs manual cleanup)

BEGIN;

-- Manual cleanup of zap_metadata (no FK)
DELETE FROM zap_metadata 
WHERE event_id IN (
  SELECT e.id 
  FROM events e
  LEFT JOIN profile_search ps ON ps.pubkey = e.pubkey
  WHERE COALESCE(ps.follower_count, 0) < 5
);

-- Delete events (will cascade to event_tags, event_refs, follows, follow_lists)
DELETE FROM events 
WHERE pubkey IN (
  SELECT DISTINCT e.pubkey
  FROM events e
  LEFT JOIN profile_search ps ON ps.pubkey = e.pubkey  
  WHERE COALESCE(ps.follower_count, 0) < 5
);

-- Clean up orphaned crawl_state entries
DELETE FROM crawl_state
WHERE author_pubkey NOT IN (SELECT DISTINCT pubkey FROM events);

COMMIT;
*/

-- Summary after deletion (run after uncommenting the purge block)
SELECT 
  'After purge:' as phase,
  (SELECT COUNT(*) FROM events) as total_events,
  (SELECT COUNT(DISTINCT pubkey) FROM events) as total_authors,
  (SELECT pg_size_pretty(pg_database_size('nostr_api'))) as db_size;

-- Recommend VACUUM FULL to reclaim disk space after large deletion
SELECT 'Run VACUUM FULL to reclaim disk space after purge' as recommendation;