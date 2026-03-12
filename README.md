# nostr-api

Nostr event ingestion and stats API. Connects to configurable relays, stores every event in PostgreSQL with full indexing, and caches aggregate stats in Redis.

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

## Setup

```bash
cp .env.example .env
# Edit .env with your database/redis/relay configuration

# Requires PostgreSQL and Redis running
cargo run
```

## API Endpoints

| Method | Path | Description |
|--------|------|-------------|
| GET | `/health` | Health check |
| GET | `/api/v1/stats` | Global stats (cached) |
| GET | `/api/v1/events` | Query events with filters |
| GET | `/api/v1/events/{id}` | Get event by ID |

### Query Parameters (`/api/v1/events`)

| Param | Type | Description |
|-------|------|-------------|
| `pubkey` | string | Filter by author public key |
| `kind` | int | Filter by event kind |
| `since` | int | Events created after (unix timestamp) |
| `until` | int | Events created before (unix timestamp) |
| `search` | string | Full-text search on content |
| `limit` | int | Max results (default 50, max 500) |
| `offset` | int | Pagination offset |

## Database Schema

**events** — one row per Nostr event, deduplicated by event ID. Includes JSONB `tags` column with GIN index and a generated `content_tsv` column for full-text search.

**event_tags** — normalized tag extraction for fast lookups (e.g., find all events mentioning a pubkey, all events with a hashtag).

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `DATABASE_URL` | `postgres://dev:dev@localhost:5432/nostr_api` | PostgreSQL connection string |
| `REDIS_URL` | `redis://127.0.0.1:6379` | Redis connection string |
| `LISTEN_ADDR` | `0.0.0.0:8000` | API server bind address |
| `RELAY_URLS` | 5 popular relays | Comma-separated relay WebSocket URLs |
| `RELAY_INDEXERS` | damus/primal/coracle/nos | Indexer relays queried for kind-10002 relay lists |
| `ENABLE_RELAY_DISCOVERY` | `true` | Toggle dynamic relay discovery on startup |
| `RELAY_DISCOVERY_TARGET` | `25` | Number of relays to keep from discovery before falling back to `RELAY_URLS` |
| `INGESTION_SINCE` | current time | Only ingest events after this unix timestamp |
| `RUST_LOG` | `nostr_api=info` | Log level filter |

### Relay discovery

When `ENABLE_RELAY_DISCOVERY` is true, the ingester queries each `RELAY_INDEXERS` endpoint for kind-10002 relay lists (NIP-65), counts how often every relay appears, and keeps the top `RELAY_DISCOVERY_TARGET` entries. Those relays are merged with `RELAY_URLS` to ensure we always fall back to the configured baseline.
