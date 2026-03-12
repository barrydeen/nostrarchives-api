# nostrarchives-api

Nostr event ingestion and REST API. Connects to configurable relays, stores every event in PostgreSQL with full indexing, and caches aggregate stats in Redis. Powers [nostrarchives.com](https://nostrarchives.com).

## Architecture

```
[Relay 1] ──┐
[Relay 2] ──┤──> [mpsc channel] ──> [Ingestion Worker] ──> [PostgreSQL]
[Relay 3] ──┘                                          └──> [Redis Counters]
                                                              ↑
                                [axum API Server] ────────────┘
```

- **Relay connections** run as independent tokio tasks with auto-reconnect and exponential backoff
- **Event processing** funnels through a single worker via bounded channel (backpressure built in)
- **PostgreSQL** stores events with indexes on pubkey, kind, created_at, relay, tags (GIN), and full-text search (tsvector)
- **Redis** caches running counters: total events, unique pubkeys (HyperLogLog), events by kind, ingestion rate

## Features

- Full Nostr event ingestion from multiple relays with deduplication
- Full-text search across event content
- Social graph tracking (follows/followers)
- Trending notes (by likes, zaps — all-time and rolling 24h)
- Intelligent historical crawler with author prioritization by follower count
- Dynamic relay discovery via NIP-65
- Profile search with fuzzy matching (trigram indexes)
- Thread traversal and interaction counts
- NIP-19 entity resolution (npub, note, nprofile, nevent)

## Prerequisites

- **Rust** (stable, 1.75+)
- **PostgreSQL** (14+)
- **Redis** (6+)

## Setup

```bash
# Clone the repo
git clone https://github.com/barrydeen/nostrarchives-api.git
cd nostrarchives-api

# Configure environment
cp .env.example .env
# Edit .env with your database/redis/relay configuration

# Create the database
createdb nostr_api

# Run (migrations apply automatically on startup)
cargo run
```

## API Endpoints

| Method | Path | Description |
|--------|------|-------------|
| GET | `/health` | Health check |
| GET | `/v1/stats` | Global stats (cached) |
| GET | `/v1/events` | Query events with filters |
| GET | `/v1/events/{id}` | Get event by ID |
| GET | `/v1/events/{id}/thread` | Thread context (ancestors + replies/reactions/etc.) |
| GET | `/v1/events/{id}/interactions` | Lightweight interaction counts |
| GET | `/v1/events/{id}/refs/{ref_type}` | Events referencing the target by type |
| GET | `/v1/social/{pubkey}` | Follow/follower counts + lists for a pubkey |
| GET | `/v1/notes/likes/top` | Top liked notes (all time) |
| GET | `/v1/notes/likes/top/today` | Top liked notes (rolling 24h) |
| GET | `/v1/notes/zaps/top` | Top zapped notes (all time) |
| GET | `/v1/notes/zaps/top/today` | Top zapped notes (rolling 24h) |
| GET | `/v1/notes/trending/{metric}/{range}` | Unified trending endpoint |
| GET | `/v1/crawler/stats` | Crawler progress and queue stats |

### Query Parameters (`/v1/events`)

| Param | Type | Description |
|-------|------|-------------|
| `pubkey` | string | Filter by author public key |
| `kind` | int | Filter by event kind |
| `since` | int | Events created after (unix timestamp) |
| `until` | int | Events created before (unix timestamp) |
| `search` | string | Full-text search on content |
| `limit` | int | Max results (default 50, max 500) |
| `offset` | int | Pagination offset |

### Query Parameters (`/v1/events/{id}/thread` and `/v1/events/{id}/refs/{ref_type}`)

| Param | Type | Description |
|-------|------|-------------|
| `limit` | int | Max related events returned (default 50, max 500) |
| `ref_type` | string | `refs` endpoint only — one of `reply`, `reaction`, `repost`, `zap`, `mention`, `root` |

### Query Parameters (`/v1/social/{pubkey}`)

| Param | Type | Description |
|-------|------|-------------|
| `follows_limit` | int | Max follow entries returned (default 100, max 500) |
| `followers_limit` | int | Max follower entries returned (default 100, max 500) |
| `follows_offset` | int | Offset into the follow list (default 0) |
| `followers_offset` | int | Offset into the follower list (default 0) |

### Query Parameters (`/v1/notes/.../top`)

| Param | Type | Description |
|-------|------|-------------|
| `limit` | int | Max notes returned (default 100, max 100) |
| `offset` | int | Pagination offset (default 0) |

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `DATABASE_URL` | `postgres://dev:dev@localhost:5432/nostr_api` | PostgreSQL connection string |
| `REDIS_URL` | `redis://127.0.0.1:6379` | Redis connection string |
| `LISTEN_ADDR` | `0.0.0.0:8000` | API server bind address |
| `RELAY_URLS` | 5 popular relays | Comma-separated relay WebSocket URLs |
| `RELAY_INDEXERS` | damus/primal/coracle/nos | Indexer relays queried for kind-10002 relay lists |
| `ENABLE_RELAY_DISCOVERY` | `true` | Toggle dynamic relay discovery on startup |
| `RELAY_DISCOVERY_TARGET` | `25` | Number of relays to keep from discovery |
| `ENABLE_SOCIAL_GRAPH_BOOTSTRAP` | `true` | Query follow lists at boot and hydrate the social graph |
| `INGESTION_SINCE` | current time | Only ingest events after this unix timestamp |
| `ENABLE_CRAWLER` | `false` | Enable intelligent historical note crawler |
| `CRAWLER_BATCH_SIZE` | `10` | Number of authors to crawl per batch |
| `CRAWLER_EVENTS_PER_AUTHOR` | `200` | Max events to fetch per author |
| `CRAWLER_REQUEST_DELAY_MS` | `500` | Delay between crawler relay requests |
| `RUST_LOG` | `nostr_api=info` | Log level filter |

### Relay Discovery

When `ENABLE_RELAY_DISCOVERY` is true, the ingester queries each `RELAY_INDEXERS` endpoint for kind-10002 relay lists (NIP-65), counts how often every relay appears, and keeps the top `RELAY_DISCOVERY_TARGET` entries. Those relays are merged with `RELAY_URLS` to ensure we always fall back to the configured baseline.

### Social Graph Bootstrap

With `ENABLE_SOCIAL_GRAPH_BOOTSTRAP` enabled, startup runs a best-effort crawl across the discovered relays for kind-3 contact lists, normalizes the `p` tags into `follows` rows, and keeps the graph in sync by replaying newer lists through the same path when new events arrive.

### Intelligent Crawler

When `ENABLE_CRAWLER` is true, the system backfills historical notes for known authors. Authors are prioritized by follower count into tiers:

| Tier | Followers | Recrawl Interval |
|------|-----------|-----------------|
| 1 | 1,000+ | More frequent |
| 2 | 100+ | Standard |
| 3 | 10+ | Less frequent |
| 4 | < 10 | Least frequent |

The crawler fetches new notes since the last crawl, performs historical backfill, and (for Tier 1-2) fetches engagement data (reactions/reposts/zaps). Progress is tracked in the `crawl_state` table with `FOR UPDATE SKIP LOCKED` for concurrency safety.

## Database Schema

Migrations are in `migrations/` and run automatically on startup:

- **events** — one row per Nostr event, deduplicated by event ID. Includes JSONB `tags` column with GIN index and a generated `content_tsv` column for full-text search.
- **event_tags** — normalized tag extraction for fast lookups (e.g., find all events mentioning a pubkey).
- **event_refs** — directional links between events by relationship type (reply, reaction, repost, zap, mention, root).
- **follows / follow_lists** — social graph edges derived from kind-3 contact list events.
- **crawl_state** — per-author crawler progress tracking.
- **profile_search** — materialized view with pre-computed profile metadata, follower counts, and engagement scores for fast search.

## Contributing

Contributions are welcome! Please open an issue or submit a pull request.

## License

MIT
