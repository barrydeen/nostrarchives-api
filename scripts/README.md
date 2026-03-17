# Database Management Scripts

## Follower-Based Data Filtering

### Overview

To manage database size, the nostr-api now filters events based on author follower count. This prevents storing content from accounts with minimal social presence while preserving the essential social graph data.

### Configuration

Set the minimum follower threshold and cache refresh via environment variables:

```bash
# .env
MIN_FOLLOWER_THRESHOLD=5         # Default: 5 followers minimum  
FOLLOWER_CACHE_REFRESH_SECS=3600 # Default: refresh every hour
```

### High-Performance Caching

The application now uses an in-memory HashSet cache for **O(1) follower threshold lookups** instead of database queries on every event. This provides massive performance gains for high-throughput ingestion:

- **Before:** Database query per event (~1-10ms latency per event)
- **After:** HashSet lookup per event (~0.001ms, no database hit) 
- **Refresh:** Cache updates hourly with latest follower data
- **Memory:** ~2-5MB for qualified pubkeys (40k+ accounts)
- **Monitoring:** `/v1/stats/follower-cache` endpoint for cache statistics

### Scripts

#### `purge_low_followers.sql`

**One-time purge** of existing data from authors with fewer than the minimum followers.

⚠️ **DESTRUCTIVE OPERATION** - Review carefully before running.

```bash
# 1. Preview what will be deleted (safe)
psql -h 46.225.169.182 -U nostr_api -d nostr_api -f purge_low_followers.sql

# 2. Uncomment the purge block in the SQL file, then run
psql -h 46.225.169.182 -U nostr_api -d nostr_api -f purge_low_followers.sql

# 3. Reclaim disk space after purge
psql -h 46.225.169.182 -U nostr_api -d nostr_api -c "VACUUM FULL;"
```

**What it deletes:**
- Events from authors with <5 followers (except profiles and follow lists)
- Associated event_tags, event_refs, follows, zap_metadata (via CASCADE)
- Orphaned crawl_state entries

**What it preserves:**
- Profile events (kind 0) - needed for social graph
- Follow list events (kind 3) - needed for social graph  
- All data from authors with ≥5 followers

#### `daily_cleanup.sql` & `daily_cleanup.sh`

**Daily maintenance** to remove data from accounts that drop below the follower threshold.

```bash
# Manual run
./scripts/daily_cleanup.sh

# Add to crontab for daily execution at 2 AM
crontab -e
# Add: 0 2 * * * /opt/apps/nostr-api/scripts/daily_cleanup.sh
```

The script only runs cleanup when there are >100 events to remove, preventing log spam.

### Application Changes

The application now filters events at ingestion time using a high-performance cache:

1. **In-memory cache** - qualified pubkeys loaded into HashSet for O(1) lookups
2. **Real-time ingestion** (`RelayIngester`) - cache check before `insert_event()` 
3. **Historical crawler** (`HybridCrawler`) - same cache-based filtering
4. **Profile/Follow events** - always allowed (kinds 0, 3) to maintain social graph
5. **New users** - allowed until they get profile data (avoids blocking newcomers)
6. **Auto-refresh** - cache updates every hour with latest follower counts

### Monitoring

Check filtering effectiveness:

```sql
-- Current database size
SELECT pg_size_pretty(pg_database_size('nostr_api'));

-- Author distribution by follower count  
SELECT 
  CASE 
    WHEN ps.follower_count = 0 THEN '0 followers'
    WHEN ps.follower_count BETWEEN 1 AND 4 THEN '1-4 followers'  
    WHEN ps.follower_count BETWEEN 5 AND 9 THEN '5-9 followers'
    WHEN ps.follower_count BETWEEN 10 AND 99 THEN '10-99 followers'
    WHEN ps.follower_count BETWEEN 100 AND 999 THEN '100-999 followers'
    ELSE '1000+ followers'
  END as bracket,
  COUNT(DISTINCT e.pubkey) as num_authors
FROM events e
LEFT JOIN profile_search ps ON ps.pubkey = e.pubkey
GROUP BY bracket 
ORDER BY MIN(COALESCE(ps.follower_count, 0));

-- Events by follower bracket
SELECT 
  CASE 
    WHEN COALESCE(ps.follower_count, 0) < 5 THEN 'Below threshold'
    ELSE 'Above threshold'
  END as status,
  COUNT(e.id) as event_count,
  ROUND(100.0 * COUNT(e.id) / (SELECT COUNT(*) FROM events), 1) as pct
FROM events e
LEFT JOIN profile_search ps ON ps.pubkey = e.pubkey
GROUP BY status;
```

### Recovery

If you need to restore data or change the threshold:

1. Set `MIN_FOLLOWER_THRESHOLD=0` to disable filtering
2. Re-run the crawler to backfill data for previously filtered accounts
3. Adjust threshold and run cleanup as needed