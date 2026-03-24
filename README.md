# nostrarchives-api

Nostr event ingestion, REST API, and WebSocket relay service. Connects to configurable relays, stores events in PostgreSQL with full indexing, and caches aggregate stats in Redis. Powers [nostrarchives.com](https://nostrarchives.com).

The frontend lives at [nostrarchives-frontend](https://github.com/barrydeen/nostrarchives-frontend).

## Prerequisites

- **Rust** (stable, 1.75+)
- **PostgreSQL** (14+)
- **Redis** (6+)

## Installation

```bash
git clone https://github.com/barrydeen/nostrarchives-api.git
cd nostrarchives-api

cp .env.example .env
# Edit .env — at minimum set DATABASE_URL and REDIS_URL

createdb nostr_api

# Run (migrations apply automatically on startup)
cargo run
```

### Build Commands

```bash
cargo build                          # debug build
cargo build --release                # production build
cargo run                            # run (loads .env automatically)
cargo run --bin purge_non_wot        # run purge utility
cargo check                          # type check without building
cargo clippy                         # lints
cargo fmt                            # format
cargo test                           # tests
```

## Deployment

The production setup runs on a Hetzner server behind nginx. The service is managed via systemd.

```bash
# On the server
cd /opt/apps/nostr-api
git pull origin main
cargo build --release
systemctl restart nostr-api
```

The service listens on four ports (see below). Nginx reverse-proxies these to public subdomains:

| Subdomain | Port | Service |
|-----------|------|---------|
| `api.nostrarchives.com` | 8000 | REST API + live metrics WebSocket |
| `relay.nostrarchives.com` | 8001 | NIP-50 search relay + feed WebSockets |
| `scheduler.nostrarchives.com` | 8002 | Scheduler relay (future-dated events) |
| `indexer.nostrarchives.com` | 8003 | Indexer relay (kinds 0, 3, 10002 only) |

## REST API Endpoints

All endpoints are rate-limited at 120 req/min per IP unless noted. Whitelist IPs via `RATELIMIT_WHITELIST`.

### Health & Stats

| Method | Path | Description |
|--------|------|-------------|
| GET | `/health` | Health check (not rate-limited) |
| GET | `/v1/stats` | Global stats (total events, pubkeys, events by kind) |
| GET | `/v1/stats/daily` | Daily network stats (DAU, posts, sats) |
| GET | `/v1/stats/follower-cache` | Cache monitoring (WoT, follower, profile search) |
| GET | `/v1/crawler/stats` | Crawler progress and queue stats |

### Events

| Method | Path | Description |
|--------|------|-------------|
| GET | `/v1/events?pubkey=&kind=&since=&until=&search=&limit=&offset=` | Query events with filters |
| GET | `/v1/events/{id}` | Single event by ID |
| GET | `/v1/events/{id}/thread` | Thread context (ancestors + replies + reactions) |
| GET | `/v1/events/{id}/interactions` | Lightweight interaction counts |
| GET | `/v1/events/{id}/refs/{ref_type}` | Events referencing target (`reply`/`reaction`/`repost`/`zap`/`mention`/`root`) |
| GET | `/v1/pages/note/{id}` | Note detail page (event + profiles + stats) |

### Profiles & Social

| Method | Path | Description |
|--------|------|-------------|
| GET | `/v1/social/{pubkey}` | Follow/follower counts and lists |
| POST | `/v1/profiles/metadata` | Batch fetch metadata for multiple pubkeys |
| GET | `/v1/profiles/{pubkey}/notes` | Root notes by author |
| GET | `/v1/profiles/{pubkey}/replies` | Replies by author |
| GET | `/v1/profiles/{pubkey}/zaps/sent` | Zaps sent by pubkey |
| GET | `/v1/profiles/{pubkey}/zaps/received` | Zaps received by pubkey |
| GET | `/v1/profiles/{pubkey}/zap-stats` | Zap statistics (total sats, count) |

### Search

| Method | Path | Description |
|--------|------|-------------|
| GET | `/v1/search?q=&type=&limit=&offset=` | Full-text search (profiles + notes) |
| GET | `/v1/search/suggest?q=&limit=` | Autocomplete suggestions |
| GET | `/v1/notes/search?q=&author=&reply_to=&order=` | Advanced note search |

### Trending & Leaderboards

| Method | Path | Description |
|--------|------|-------------|
| GET | `/v1/notes/top?metric=&range=&limit=&offset=` | Top notes by metric × range |
| GET | `/v1/notes/trending?limit=` | Trending notes (composite score) |
| GET | `/v1/users/new?limit=` | Recently joined users |
| GET | `/v1/users/trending?limit=` | Trending users (follower gain) |
| GET | `/v1/users/zappers?direction=&range=&limit=` | Top zappers (sent/received) |
| GET | `/v1/hashtags/trending?limit=` | Trending hashtags |
| GET | `/v1/hashtags/{tag}/notes?limit=&offset=` | Notes for a hashtag |

### Analytics

| Method | Path | Description |
|--------|------|-------------|
| GET | `/v1/analytics/daily?days=30` | Daily analytics (DAU, posts, zaps) |
| GET | `/v1/analytics/top-posters?range=&limit=` | Top posting authors |
| GET | `/v1/analytics/most-liked?range=&limit=` | Most liked authors |
| GET | `/v1/analytics/most-shared?range=&limit=` | Most shared authors |
| GET | `/v1/clients/leaderboard?limit=` | Top Nostr clients |
| GET | `/v1/relays/leaderboard?limit=` | Top relays |

### WebSocket

| Path | Description |
|------|-------------|
| `/v1/ws/live-metrics` | Live metrics stream — JSON `{"online","sats","notes"}` (not rate-limited) |

## WebSocket Relay (Port 8001)

NIP-50 compatible search relay with extended feed endpoints. All feeds use Nostr REQ/CLOSE/EVENT protocol.

| Path | Description |
|------|-------------|
| `/` | NIP-50 full-text search (kinds 0 + 1). Supports `#t` tag filters, NIP-19 entity resolution |
| `/notes/trending/{metric}/{range}` | Trending notes. metric: `reactions`/`replies`/`reposts`/`zaps`. range: `today`/`7d`/`30d`/`1y`/`all` |
| `/users/upandcoming` | Emerging users (NIP-51 kind-30000 people list) |
| `/profiles/followers` | Follower profiles for a pubkey (pass pubkey via `authors` filter in REQ) |
| `/profiles/{note_type}/{metric}` | Ranked notes for a profile. note_type: `root`/`replies`. metric: `likes`/`reposts`/`zaps`/`replies` |
| `/hashtags/{variant}` | Hashtag feeds. variant: `trending` (top 100) or `all` (count > 5) |

## Scheduler Relay (Port 8002)

Accepts future-dated events and publishes them at their `created_at` time.

- Requires NIP-42 authentication on connect
- Events must be 60 seconds to 90 days in the future
- Max 100 pending events per pubkey
- Publishes to author's NIP-65 write relays (or top 20 relays)
- Supports NIP-09 deletion of pending events

## Indexer Relay (Port 8003)

Restricted read-only relay for efficient metadata discovery.

- Only serves kinds 0, 3, 10002
- `authors` filter required (1–500 hex pubkeys)
- 30 requests/min per IP
- Max 500 results per request

## Crawler Modes

The crawler backfills historical events. Controlled by `ENABLE_CRAWLER` and `CRAWL_MODE`.

### Hybrid (default, `CRAWL_MODE=hybrid`)

Combines three strategies:

1. **Negentropy** — Set-reconciliation against relays to efficiently find missing events. Runs every `NEGENTROPY_SYNC_INTERVAL_SECS` (default 300s).
2. **NIP-65 relay routing** — Fetches each author from their own write relays (`CRAWLER_USE_RELAY_LISTS=true`).
3. **Legacy time-range** — Fallback per-author fetch with configurable batch size and delay.

### Negentropy-only (`CRAWL_MODE=negentropy_only`)

Simple per-author negentropy sync against pinned relays. Configure via `NEGENTROPY_PINNED_RELAYS`.

### Author Prioritization

Authors are tiered by follower count for crawl scheduling:

| Tier | Followers | Priority |
|------|-----------|----------|
| 1 | 1,000+ | Highest |
| 2 | 100+ | Standard |
| 3 | 10+ | Lower |
| 4 | < 10 | Lowest |

Progress tracked in `crawl_state` table with `FOR UPDATE SKIP LOCKED` for concurrency.

## Event Processing

Events are routed by kind on ingestion:

| Kind | Behavior |
|------|----------|
| 0 (metadata) | Stored if author passes WoT check or already has events |
| 1 (note) | WoT-gated. Stores event, inserts refs, increments reply_count on target |
| 3 (contact list) | Always processed. Upserts social graph only (not stored as event) |
| 6/16 (repost) | Counter-only. Increments repost_count, event not stored |
| 7 (reaction) | Counter-only. Increments reaction_count, event not stored |
| 9735 (zap) | Always stored. Extracts bolt11 amount, increments zap counters |
| 10002 (relay list) | Always processed. Upsert only |

### Web of Trust (WoT)

A pubkey passes if followed by ≥ `WOT_THRESHOLD` (default 21) pubkeys that themselves have ≥ `MIN_FOLLOWER_THRESHOLD` (default 5) followers. Refreshes every 15 min.

## Environment Variables

### Core

| Variable | Default | Description |
|----------|---------|-------------|
| `DATABASE_URL` | `postgres://dev:dev@localhost:5432/nostr_api` | PostgreSQL connection |
| `REDIS_URL` | `redis://127.0.0.1:6379` | Redis connection |
| `LISTEN_ADDR` | `0.0.0.0:8000` | REST API bind address |
| `WS_LISTEN_ADDR` | `0.0.0.0:8001` | Search/feed relay bind address |
| `SCHEDULER_WS_LISTEN_ADDR` | `0.0.0.0:8002` | Scheduler relay bind address |
| `INDEXER_WS_LISTEN_ADDR` | `0.0.0.0:8003` | Indexer relay bind address |
| `RUST_LOG` | `nostr_api=info` | Log level |
| `RATELIMIT_WHITELIST` | — | Comma-separated IPs to bypass rate limiting |

### Relays & Ingestion

| Variable | Default | Description |
|----------|---------|-------------|
| `RELAY_URLS` | 5 popular relays | Comma-separated relay WebSocket URLs |
| `RELAY_INDEXERS` | damus/primal/coracle/nos | Relays queried for NIP-65 relay lists |
| `ENABLE_RELAY_DISCOVERY` | `true` | Dynamic relay discovery on startup |
| `RELAY_DISCOVERY_TARGET` | `25` | Number of relays to keep from discovery |
| `INGESTION_SINCE` | current time | Only ingest events after this unix timestamp |
| `ENABLE_SOCIAL_GRAPH_BOOTSTRAP` | `true` | Query follow lists at boot |

### Crawler

| Variable | Default | Description |
|----------|---------|-------------|
| `ENABLE_CRAWLER` | `true` | Enable historical backfill |
| `CRAWL_MODE` | `hybrid` | `hybrid` or `negentropy_only` |
| `NEGENTROPY_ENABLED` | `true` | Enable negentropy sync in hybrid mode |
| `NEGENTROPY_SYNC_INTERVAL_SECS` | `300` | Negentropy sync interval |
| `NEGENTROPY_MAX_RELAYS` | `20` | Max relays per negentropy cycle |
| `NEGENTROPY_RELAY_URLS` | — | Primary negentropy relays (comma-separated) |
| `NEGENTROPY_PINNED_RELAYS` | 7 well-known relays | Pinned relays for negentropy_only mode |
| `CRAWLER_USE_RELAY_LISTS` | `true` | Route crawls via NIP-65 write relays |
| `CRAWLER_BATCH_SIZE` | `10` | Authors per batch |
| `CRAWLER_EVENTS_PER_AUTHOR` | `500` | Max events per author |
| `CRAWLER_REQUEST_DELAY_MS` | `500` | Delay between relay requests |
| `CRAWLER_POLL_INTERVAL_SECS` | `30` | Polling interval |
| `CRAWLER_MAX_CONCURRENCY` | `3` | Concurrent crawler tasks |
| `CRAWLER_MAX_RELAY_POOL_SIZE` | `50` | Max concurrent relay connections |

### WoT & Filtering

| Variable | Default | Description |
|----------|---------|-------------|
| `WOT_THRESHOLD` | `21` | Min qualified followers to pass WoT |
| `MIN_FOLLOWER_THRESHOLD` | `5` | Min followers a follower must have |
| `WOT_REFRESH_SECS` | `900` | WoT cache refresh interval |

### Features

| Variable | Default | Description |
|----------|---------|-------------|
| `ENABLE_SCHEDULER` | `false` | Enable scheduler relay |
| `ENABLE_INDEXER` | `true` | Enable indexer relay |
| `ENABLE_FEEDS` | `true` | Enable hashtag feed generation |
| `ONDEMAND_FETCH_ENABLED` | `true` | Fetch missing events from relays on API miss |

## Database

Migrations in `migrations/` run automatically on startup. Key tables:

- **events** — one row per event. JSONB `tags` with GIN index, generated `content_tsv` for FTS.
- **event_refs** — directional edges by type (reply/reaction/repost/zap/mention/root)
- **follows / follow_lists** — social graph from kind-3 events
- **crawl_state** — per-author crawler progress with priority tiers
- **relay_lists** — NIP-65 relay URLs per author
- **zap_metadata** — parsed zap receipts (sender, receiver, amount, timestamp)
- **scheduled_events** — future-dated events pending publication
- **daily_analytics** — aggregated daily stats
- **profile_search** — materialized view for fast profile lookups

## License

MIT
