# Local test environment for the bot ban/purge work.
#
#   docker compose up -d postgres redis
#   set -a; . scripts/nspam/local_env.sh; set +a
#   cargo run --bin nostr-api
#
# Everything outbound is off so a local run never connects to real relays or
# pulls the live firehose into the test database. The WoT and follower gates are
# disabled too, otherwise hand-seeded test events get rejected at ingestion.

DATABASE_URL=postgres://dev:dev@localhost:5432/nostr_api
REDIS_URL=redis://127.0.0.1:6379
LISTEN_ADDR=0.0.0.0:8000
WS_LISTEN_ADDR=0.0.0.0:8001
RUST_LOG=nostr_api=info

# No outbound relay traffic.
RELAY_URLS=
ENABLE_CRAWLER=false
ENABLE_RELAY_DISCOVERY=false
ENABLE_SOCIAL_GRAPH_BOOTSTRAP=false
NEGENTROPY_ENABLED=false
ONDEMAND_FETCH_ENABLED=false

# Extra listeners off — not needed to exercise ban/purge.
ENABLE_INDEXER=false
ENABLE_SCHEDULER=false
ENABLE_FEEDS=false

# Ingestion gates off so seeded test data survives.
WOT_THRESHOLD=0
MIN_FOLLOWER_THRESHOLD=0
