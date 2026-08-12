# Deploying bot moderation to production

Runbook for deploying PR #121 and running the first bot purge. Written for an
operator or agent with SSH access to the Hetzner hosts.

Read this whole file before starting. The purge is irreversible except through
the archives it writes, and step ordering is load-bearing in two places.

## Topology

| host | role | relevance |
|---|---|---|
| `hetzner-backends` (157.90.21.21) | Rust API (systemd) + Redis | deploy target; **run everything from here** |
| `hetzner-db` (46.225.169.182) | PostgreSQL 16 | only reachable from backends |

Postgres is not reachable from your laptop. The scorer and the purge both talk
to the database directly, so both run on `hetzner-backends`.

## Rules

- Deploy by `git pull` on the server. Never `scp`/`rsync`.
- Never edit code directly on a server.
- Preserve `.env` — it is not in git.

---

## 1. Pre-flight

```bash
ssh hetzner-backends
cd /opt/apps/nostr-api

# Confirm clean checkout — a dirty tree means someone edited on the server.
git status --porcelain          # expect empty
git log --oneline -1

# Back up .env before anything touches it.
cp .env ~/nostr-api.env.$(date +%F)

# Prerequisites for the scorer.
python3 --version               # need 3.10+
df -h /                         # need ~2GB: venv ~350MB, model ~10MB, archives
curl -sI https://huggingface.co | head -1   # model downloads from here
```

Take a database backup. This deletes a large fraction of the corpus and the
JSONL archives only cover what the purge itself removes:

```bash
ssh hetzner-backends "pg_dump -h 46.225.169.182 -U nostr_api -d nostr_api -Fc \
  -f ~/nostr_api_pre_purge_$(date +%F).dump"
```

Record the starting state so you can verify afterwards:

```sql
SELECT COUNT(*) AS events, COUNT(DISTINCT pubkey) AS authors FROM events;
SELECT pg_size_pretty(pg_database_size('nostr_api'));
```

## 2. Decide on REJECT_PROXIED_EVENTS before you restart

The new binary **defaults to rejecting bridged content at ingestion**, and that
takes effect the moment the service restarts. That is the intended behaviour,
but decide deliberately:

- Want it live immediately → change nothing.
- Want to deploy the code and flip the switch separately → add
  `REJECT_PROXIED_EVENTS=false` to `.env` now, deploy, then remove it later and
  restart again.

## 3. Deploy

```bash
cd /opt/apps/nostr-api
git pull origin main
cargo build --release           # several minutes
```

**Migrations run at startup, before the service binds a port.** Three new ones
apply on first boot, and the service is down until they finish:

- `043_machine_note_flag` — full scan of every kind-1 row. Never applied in
  production before. Sub-second on 185k rows locally; scales roughly linearly.
- `044_author_spam_scores` — creates three small tables. Instant.
- `045_purge_perf_indexes` — five `CREATE INDEX CONCURRENTLY`. Does not block
  writes, but runs serially and can take minutes each on large tables.

Budget a maintenance window sized to your `events` and `zap_metadata` tables.

```bash
systemctl restart nostr-api
journalctl -u nostr-api -f      # watch for "applying migration" then "api server listening"
```

Verify:

```bash
curl -s localhost:8000/v1/stats | head -c 300
psql -h 46.225.169.182 -U nostr_api -d nostr_api \
  -c "SELECT name FROM _migrations WHERE name >= '043' ORDER BY name;"
```

Expect `043_machine_note_flag`, `044_author_spam_scores`, `045_purge_perf_indexes`.

**If ingestion breaks after this**, `REJECT_PROXIED_EVENTS=false` in `.env` plus a
restart reverts the behaviour change without touching the schema.

Let the service run normally for a few hours before purging. Confirm event
counts still climb and no errors in `journalctl`.

---

## 4. Purge, pass 1 — bridged accounts

This is the large, deterministic win. It uses NIP-48 proxy tags, not the
classifier, so there is no threshold and no model involved.

```bash
ssh hetzner-backends
cd /opt/apps/nostr-api
tmux new -s purge                # REQUIRED: this runs for hours
```

Size it first. Dry run is the default and changes nothing:

```bash
cargo run --release --bin ban_bots -- --bridged --max-fraction 1.0
```

Prints the account count, event count and the 15 largest by volume. On the
local 643k-event snapshot this was 50,962 accounts / 226,865 events (35% of the
corpus). **Production is larger — read the numbers before continuing.**

Then run it in batches. Do not attempt the whole thing in one go the first time:

```bash
mkdir -p /var/backups/nspam

# First batch: 500 accounts. Stop and inspect.
cargo run --release --bin ban_bots -- --bridged --execute \
  --max-bans 500 --max-fraction 1.0 --sleep 20 \
  --archive /var/backups/nspam/bridged-001.jsonl
```

Check the site, then continue with larger batches, incrementing the archive
filename each time. Re-running picks up where it left off — already-blocked
accounts are excluded from the selection.

### Throughput

Cost is dominated by **per-account round trips**, not events, because most
bridged accounts hold only a handful of events. Measured locally: ~50k accounts
took ~11 minutes at `--sleep 0`.

`--sleep` is the main lever. At the default 100ms, 50k accounts spend 85 minutes
sleeping versus ~1 minute of actual work. The pause exists to keep the live API
responsive:

| `--sleep` | 50k accounts |
|---|---|
| 100 (default) | ~86 min |
| 20 | ~18 min |
| 0 | ~11 min |

Start at `--sleep 20` and watch API latency. Drop to 0 only if the service is
comfortable.

Optional, and the biggest single lever if throughput matters: set
`ENABLE_CRAWLER=false` and `NEGENTROPY_ENABLED=false` in `.env` and restart for
the duration. They otherwise compete for IO and re-request deleted ids.
**Remember to restore both afterwards.**

---

## 5. Purge, pass 2 — classifier

Smaller, probabilistic, and **requires human review**. Do not automate this pass.

```bash
cd /opt/apps/nostr-api/scripts/nspam
python3 -m venv .venv
.venv/bin/pip install -r requirements.txt
export DATABASE_URL="postgres://nostr_api@46.225.169.182:5432/nostr_api"

# Must print PARITY OK. If it does not, stop — do not score.
.venv/bin/python verify_parity.py

.venv/bin/python score_authors.py --execute --sleep-ms 20
```

Review before banning anything:

```bash
.venv/bin/python review.py --scored-on replies hist
.venv/bin/python review.py --scored-on replies report --threshold 0.99 --out flagged.html
```

Copy `flagged.html` off the server and **read it**. On the local snapshot, 41
accounts were flagged at >= 0.99 and roughly 3 in 11 of the ban-eligible ones
were real people (a writer, a winery). Expect to exempt some by hand:

```sql
INSERT INTO spam_allowlist (pubkey, reason, added_by)
VALUES ('<hex pubkey>', 'reviewed: legitimate', 'ops');
```

Then confirm and purge:

```bash
.venv/bin/python review.py --scored-on replies promote --threshold 0.99 --max-bans 200
.venv/bin/python review.py --scored-on replies promote --threshold 0.99 --max-bans 200 --execute

cd /opt/apps/nostr-api
cargo run --release --bin ban_bots           # dry run
cargo run --release --bin ban_bots -- --execute --max-bans 200 \
  --archive /var/backups/nspam/classifier-001.jsonl
```

**Do not pass `--allow-out-of-distribution`.** The model is trained on replies;
scoring root notes flagged 29% of authors versus 2.3%, including accounts with
1000+ followers that were plainly human. The guard exists for a reason.

---

## 6. After the purge

Order matters — three analytics views join `profile_search`, so it refreshes first.

```bash
# 1. Redis. unique_pubkeys is a HyperLogLog: elements cannot be removed,
#    only rebuilt, so it must be deleted outright.
redis-cli DEL nostr:total_events nostr:unique_pubkeys nostr:events_by_kind
redis-cli --scan --pattern 'nostr:trending*'  | xargs -r redis-cli DEL
redis-cli --scan --pattern 'nostr:home*'      | xargs -r redis-cli DEL
redis-cli --scan --pattern 'nostr:analytics*' | xargs -r redis-cli DEL
redis-cli --scan --pattern 'nostr:ws*'        | xargs -r redis-cli DEL
redis-cli --scan --pattern 'nostr:profile*'   | xargs -r redis-cli DEL
```

```sql
-- 2. profile_search FIRST.
REFRESH MATERIALIZED VIEW CONCURRENTLY profile_search;
-- 3. Then the dependents.
REFRESH MATERIALIZED VIEW CONCURRENTLY mv_client_leaderboard;
REFRESH MATERIALIZED VIEW CONCURRENTLY mv_client_top_users;
REFRESH MATERIALIZED VIEW CONCURRENTLY mv_pubkey_first_seen;
REFRESH MATERIALIZED VIEW CONCURRENTLY mv_author_leaderboards;
REFRESH MATERIALIZED VIEW CONCURRENTLY mv_zapper_leaderboards;
REFRESH MATERIALIZED VIEW CONCURRENTLY mv_relay_leaderboard;
```

```bash
# 4. Restart so ProfileSearchCache / WotCache / FollowerCache reload.
#    Without this, purged profiles keep appearing in search for up to 24h.
systemctl restart nostr-api
```

```sql
-- 5. Reclaim. Plain VACUUM, never VACUUM FULL: FULL takes an ACCESS EXCLUSIVE
--    lock and needs as much free disk as the table. Plain VACUUM returns space
--    to Postgres's freelist for reuse — the database file will not shrink, and
--    that is expected.
VACUUM (ANALYZE) events;
VACUUM (ANALYZE) event_refs;
VACUUM (ANALYZE) note_hashtags;
VACUUM (ANALYZE) search_index;
VACUUM (ANALYZE) zap_metadata;
VACUUM (ANALYZE) seen_events;
VACUUM (ANALYZE) follows;
VACUUM (ANALYZE) follow_lists;
```

If `REFRESH` or `VACUUM` fails with *"could not resize shared memory segment ...
No space left on device"*, Postgres is running in Docker with the default 64MB
`/dev/shm`. Either set `shm_size: 1gb` on the container, or work around it:
`SET max_parallel_maintenance_workers = 0;` and `VACUUM (ANALYZE, PARALLEL 0)`.

### Verify

```sql
-- All zero.
SELECT COUNT(*) FROM events e JOIN blocked_pubkeys b USING (pubkey);
SELECT COUNT(*) FROM search_index s JOIN blocked_pubkeys b USING (pubkey);
SELECT COUNT(*) FROM follows f JOIN blocked_pubkeys b
  ON b.pubkey IN (f.follower_pubkey, f.followed_pubkey);
SELECT COUNT(*) FROM events WHERE reply_count < 0;
```

```bash
curl -s localhost:8000/v1/stats          # totals should match the database
curl -s localhost:8000/v1/notes/trending?limit=5 | head -c 200
```

Then leave it an hour and re-check that blocked authors have not returned. That
is what proves the block is holding against negentropy and the crawler.

---

## 7. Undo

Every purge writes a JSONL archive first. To reverse one:

```bash
cd /opt/apps/nostr-api/scripts/nspam
.venv/bin/python restore_archive.py --file /var/backups/nspam/bridged-001.jsonl   # dry run
.venv/bin/python restore_archive.py --file /var/backups/nspam/bridged-001.jsonl --execute --unblock
```

`--unblock` also clears `blocked_pubkeys`; without it the accounts stay blocked
and ingestion keeps rejecting them. Restore rebuilds `event_refs` and `is_reply`
from the archived tags, and the insert triggers rebuild `search_index` and
`note_hashtags`. What does not come back: `follows`/`follow_lists` (they return
when the author's kind-3 is re-ingested) and engagement counters the author
contributed to other people's notes.

Keep archives until the wave has been live a week or two, then move them off the
database host. Nothing expires them automatically.

---

## Things that will bite you

- **Run everything from `hetzner-backends`.** `hetzner-db` is not reachable
  elsewhere.
- **Use tmux.** A dropped SSH session mid-purge leaves accounts blocked but not
  purged. Harmless and resumable — re-running skips already-blocked accounts —
  but confusing if unexpected.
- **Block-then-delete ordering is not cosmetic.** Deleting without blocking is
  self-reversing: negentropy rebuilds its id set from `events`, so deleted ids
  return as `need_ids` on the next sync. `ban_bots` handles this; do not
  hand-roll deletions.
- **The 5% population guard will refuse the bridged sweep.** That is deliberate —
  bridged accounts are a large share of the corpus. Pass `--max-fraction 1.0`
  consciously, not reflexively.
- **`--max-bans` is mandatory with `--execute`.** No default.
- **One account is deliberately spared**: any author mixing bridged and native
  notes. On the local snapshot that was exactly one (SnowCait, 10 bridged / 242
  native). Leftover proxy-tagged events after the sweep are expected, not a bug.
