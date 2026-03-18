# On-Demand Relay Fetcher

The on-demand relay fetcher enables the nostr-api to dynamically fetch missing data from Nostr relays when API requests are made. This ensures that the frontend never shows blank pages if data exists on relays but isn't in our local database.

## Configuration

Environment variables:

- `ONDEMAND_FETCH_ENABLED` (default: `true`) - Enable/disable on-demand fetching
- `ONDEMAND_FETCH_TIMEOUT_MS` (default: `5000`) - Timeout per fetch operation in milliseconds  
- `ONDEMAND_FETCH_MAX_RELAYS` (default: `3`) - Maximum number of relays to try per fetch

## Architecture

### Core Component: RelayFetcher (`src/relay/fetcher.rs`)

The `RelayFetcher` service provides:

- **Event fetching**: `fetch_event_by_id()` - fetch single events by hex ID
- **Profile metadata**: `fetch_profile_metadata()` - fetch kind-0 profiles  
- **Author content**: `fetch_author_content()` - fetch kind-1 notes + kind-0/3/10002 events
- **Batch profiles**: `ensure_profiles()` - batch-fetch missing profile metadata

### Key Features

#### Request Coalescing
Multiple simultaneous requests for the same data are coalesced into a single relay fetch using `tokio::sync::Notify` to avoid duplicate network calls.

#### Relay Selection Priority
1. **Relay hints** from NIP-19 entities (nprofile, nevent)
2. **NIP-65 relay lists** from the database via `RelayRouter`
3. **Default/fallback relays** from configuration

#### Negative Caching
Failed fetches are cached in Redis for 5 minutes with key format `fetch:miss:{type}:{id}` to prevent hammering relays for non-existent data.

#### Timeout Protection
All relay operations have a 5-second timeout (configurable) to prevent API requests from hanging indefinitely.

## API Integration

### Handlers with On-Demand Fetching

#### `GET /v1/events/{id}` 
- If event not in DB → fetch from relays → return if found

#### `GET /v1/pages/note/{id}`
- If note not found → fetch from relays → re-query after fetch

#### `POST /v1/profiles/metadata`
- After DB query → identify missing profiles → batch-fetch → re-query

#### `GET /v1/profiles/{pubkey}/notes`
- If empty result AND offset=0 → fetch author content → re-query

#### `GET /v1/profiles/{pubkey}/replies`  
- If empty result AND offset=0 → fetch author content → re-query

#### `GET /v1/social/{pubkey}`
- If both follows/followers count = 0 → fetch author content (contact list) → re-query

#### `GET /v1/events/{id}/thread`
- If thread found → check for missing parent/root events → fetch missing
- If main event not found → fetch it → re-query thread

#### `GET /v1/search` & `GET /v1/search/suggest`
- NIP-19 entity resolution uses relay hints from nprofile/nevent
- 64-char hex tries event fetch first, then profile fetch

### NIP-19 Relay Hints

The search/resolve functionality extracts relay hints from:
- `nprofile1...` entities → profile metadata fetch with hints
- `nevent1...` entities → event fetch with hints  

Raw 64-character hex is ambiguous, so we try both event and profile fetching.

## Usage Examples

### Fetching a Missing Event
```bash
# Event doesn't exist in DB yet
curl http://localhost:8000/v1/events/abcd1234...

# → RelayFetcher.fetch_event_by_id() called
# → Connects to default relays or relay hints  
# → Sends REQ filter: {"ids": ["abcd1234..."]}
# → Stores received event → returns to user
```

### Profile Resolution with Relay Hints
```bash  
# Search for nprofile with relay hints
curl "http://localhost:8000/v1/search?q=nprofile1..."

# → nip19::decode() extracts pubkey + relay URLs
# → RelayFetcher.fetch_profile_metadata() uses hint relays
# → Connects to hint relays first, then defaults
# → Fetches kind-0 metadata → stores → returns resolved entity
```

### Author Content Discovery
```bash
# Profile has no notes/replies in DB
curl http://localhost:8000/v1/profiles/pubkey123/notes

# → Empty result from DB
# → RelayFetcher.fetch_author_content() called
# → Fetches kinds [0,1,3,10002] from relays  
# → Stores ~100 events → re-queries → returns populated feed
```

## Error Handling

- **Connection failures**: Logged as warnings, try next relay
- **Timeout**: Operations abort after timeout, return existing DB data  
- **Parse errors**: Invalid events logged, processing continues
- **Negative cache**: Prevent retry loops for truly missing data
- **Graceful degradation**: API never fails due to fetch issues

## Monitoring

- **Tracing logs**: All fetch operations logged with pubkey/event_id  
- **Cache stats**: Negative cache hits tracked in Redis
- **Performance**: Request coalescing reduces duplicate fetches

## Implementation Notes

- **Thread-safe**: Uses `Arc<Mutex<>>` for inflight request tracking
- **Non-blocking**: Relay fetches don't block main API response paths
- **Resilient**: Never panics, always provides fallbacks
- **Efficient**: Batch operations where possible, smart relay selection
