# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
# Build
cargo build

# Run (reads .env automatically via dotenvy)
cargo run

# Check for errors without building
cargo check

# Run clippy lints
cargo clippy

# Run a specific binary
cargo run --bin test_negentropy

# Format code
cargo fmt

# Run tests
cargo test

# Run a single test
cargo test <test_name>
```

There are no automated tests beyond `cargo test`. The `src/bin/test_negentropy.rs` binary is a manual integration test for the negentropy sync protocol.

## Architecture Overview

This is a Rust/Tokio async service that ingests Nostr events from multiple relays, stores them in PostgreSQL, and serves them via a REST API. All modules are declared in `src/main.rs`.

### Process Topology

On startup, `main.rs` spins up several concurrent subsystems, all sharing `Arc`-wrapped state:

1. **RelayIngester** (`src/relay/ingester.rs`) — connects to each relay URL as an independent tokio task, subscribes to live events via WebSocket, and routes inbound events through `EventRepository::insert_event`. Uses a bounded `mpsc` channel to funnel events through a single processing worker (backpressure).

2. **MetadataResolver** (`src/relay/metadata.rs`) — receives pubkey hints over an `mpsc` channel from the ingester and fetches kind-0 metadata for new pubkeys.

3. **HybridCrawler** (`src/crawler/orchestrator.rs`) — historical backfill engine. Combines:
   - Negentropy set-reconciliation sync (bulk diff against relays)
   - NIP-65 relay list routing (fetch each author from their own write relays)
   - Legacy per-author time-range fetch fallback

4. **REST API** (`src/api/`) — axum server on `LISTEN_ADDR` (default `:8000`). Rate-limited at 120 req/min per IP. IP whitelist via `RATELIMIT_WHITELIST`.

5. **WebSocket search relay** (`src/ws/mod.rs`) — NIP-50 compatible endpoint on `WS_LISTEN_ADDR` (default `:8001`). Also serves feed endpoints for trending notes, followers, and ranked profile notes.

6. **Indexer relay** (`src/indexer/mod.rs`) — restricted read-only WebSocket relay on `:8003` (default). Serves only kinds 0, 3, 10002. Requires `authors` filter. Enabled via `ENABLE_INDEXER=true`.

7. **Scheduler relay** (`src/scheduler/mod.rs`) — accepts future-dated events and publishes them at `created_at` time on `:8002` (default). Enabled via `ENABLE_SCHEDULER=true`.

8. **Background tasks** (inline in `main.rs`):
   - `profile_search` materialized view refresh every 5 minutes
   - Analytics materialized views refresh every 30 minutes
   - Daily analytics computation at midnight UTC (backfills last 30 days on startup)

### Shared State

`AppState` (defined in `src/api/mod.rs`) wraps:
- `EventRepository` — all DB access; holds `PgPool`, `FollowerCache`, and `WotCache`
- `StatsCache` — Redis-backed counters and JSON response caching
- `CrawlQueue` — crawler work queue (optional, `None` when crawler disabled)
- `RelayFetcher` — on-demand fetcher for missing events/profiles
- `ProfileSearchCache` — in-memory cache of `profile_search` MV for zero-DB-hit searches

### Event Processing (v2 routing in `src/db/repository.rs`)

`insert_event` applies kind-based routing:
- **Kind 0** (metadata): stored only if author passes WoT check OR already has events
- **Kind 1** (note): WoT-gated; stores event, inserts `event_refs`, increments `reply_count` on target
- **Kind 3** (contact list): always processed; upserts social graph rows only (not stored as an event)
- **Kind 6/16** (repost): counter-only; increments `repost_count`, event not stored
- **Kind 7** (reaction): counter-only; increments `reaction_count`, event not stored
- **Kind 9735** (zap): always stored; increments zap counters and parses bolt11 for sat amount
- **Kind 10002** (relay list): always processed; upsert only

### Web of Trust (WoT)

`WotCache` (`src/wot_cache.rs`) implements a two-level follower quality check: a pubkey passes if it is followed by at least `WOT_THRESHOLD` (default 21) pubkeys that themselves have at least `MIN_FOLLOWER_THRESHOLD` (default 5) followers. Cache refreshes every `WOT_REFRESH_SECS` (default 15 min). This gates kind-0 and kind-1 ingestion to filter low-quality content.

### Crawler Architecture

The `HybridCrawler` (`src/crawler/orchestrator.rs`) coordinates:
- `NegentropySyncer` (`src/crawler/negentropy.rs`) — set-reconciliation against relay event sets
- `RelayRouter` (`src/crawler/relay_router.rs`) — looks up per-author NIP-65 write relays from the `relay_lists` table
- `CrawlQueue` (`src/crawler/queue.rs`) — priority queue in PostgreSQL using `FOR UPDATE SKIP LOCKED` for safe concurrency; authors are tiered by follower count

### Database

Migrations in `migrations/` run automatically via sqlx on startup (`db::init_pool`). Key tables:
- `events` — one row per stored event; `tags` JSONB with GIN index; `content_tsv` generated column for FTS
- `event_refs` — directional edges (reply/reaction/repost/zap/mention/root)
- `event_tags` — normalized tag rows for fast lookups
- `follows` / `follow_lists` — social graph from kind-3 events
- `crawl_state` — per-author crawler progress (last fetched timestamp, tier)
- `relay_lists` — per-author NIP-65 relay URLs
- `profile_search` — materialized view with profile metadata + follower counts + engagement scores
- `daily_analytics` / analytics materialized views — aggregated daily stats

### Key Dependencies

- `axum 0.8` with `ws` feature — HTTP + WebSocket server
- `sqlx 0.8` — async PostgreSQL (compile-time checked queries)
- `tokio-tungstenite` — outbound WebSocket relay connections
- `negentropy 0.5` — set reconciliation protocol crate
- `secp256k1` — event signature verification
- `bech32` + `hex` — NIP-19 entity decoding (`src/nip19.rs`)

## Environment

Copy `.env.example` to `.env` before first run. The most impactful non-default settings:

- `ENABLE_CRAWLER=true` — enables historical backfill (default `true` in code, `false` in example)
- `NEGENTROPY_ENABLED=true` — uses set-reconciliation for bulk sync
- `CRAWLER_USE_RELAY_LISTS=true` — routes crawl requests to each author's own write relays
- `WOT_THRESHOLD` — lower values ingest more content; higher values are more selective
- `ONDEMAND_FETCH_ENABLED=true` — fetches missing events from relays on API miss
