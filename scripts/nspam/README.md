# nspam — bot detection and banning

Detects Nostr reply-spam bots with the [`barrydeen/nspam`](https://huggingface.co/barrydeen/nspam)
classifier (v2.2), then bans them and deletes their notes.

The classifier is a **LightGBM model over sklearn-style hashed n-grams**, not a
transformer. It scores *one author* from *1–10 of their recent reply notes* and
returns a calibrated probability that the author is a reply-spammer.

## Bridged content — handled deterministically, not by the classifier

nostrarchives is for people posting on Nostr. Content relayed in from
ActivityPub/Mastodon, RSS and Bluesky is removed, and that needs no model:
NIP-48 `proxy` tags are attached by the bridge itself, so membership is a fact
about the event rather than a prediction. No threshold, no false positives.

Measured on a 643k-event corpus:

| kind | proxy-tagged events | authors |
|---|---|---|
| 0 (profiles) | 135,660 | 43,806 |
| 10002 (relay lists) | 25,671 | 25,671 |
| 1 (notes) | 40,130 | 197 |

Counting whole accounts: **50,962 authors / 226,865 events — 35% of the corpus.**
Most of it is mirrored profile metadata for Mastodon users who never posted a
note here, which is exactly the noise that makes the site feel less
human-oriented. The top 200 accounts alone hold 73,184 events.

```bash
cargo run --release --bin ban_bots -- --bridged                       # dry run
cargo run --release --bin ban_bots -- --bridged --execute --max-bans 500 \
    --max-fraction 0.5 --archive /var/backups/nspam/bridged1.jsonl
```

`--max-fraction` must be raised deliberately here: bridged accounts are a third
of the corpus, so the default 5% population guard refuses on purpose.

**Selection rule:** an account is bridged if it has proxy-tagged events *and*
zero native kind-1 notes. The native test is scoped to kind 1 because bridges
also emit kind-0 profiles and kind-10002 relay lists without proxy tags —
testing across all kinds would spare almost every mirrored account on a
technicality. Exactly one kind-1 author in the corpus mixes bridged and native
notes, and the rule excludes them.

Going forward, ingestion drops proxy-tagged events outright
(`is_proxied_event` in `src/db/repository.rs`, first check in `insert_event`).
Set `REJECT_PROXIED_EVENTS=false` to disable; it defaults to on.

## Scope and the root-note question

**Banning is author-level.** `purge_pubkey_data` deletes *every* event by a
banned pubkey — root notes, replies, profile, contact list. So a bot caught via
its replies already has all its root notes deleted. The gap is bots that *only*
post root notes and never reply: those were never scored at all.

`--notes` closes that gap:

| mode | selects | status |
|---|---|---|
| `replies` (default) | `kind=1 AND is_reply` | matches training |
| `roots` | `kind=1 AND NOT is_reply` | **out-of-distribution** |
| `all` | `kind=1` | **out-of-distribution** |

All three exclude `is_machine_note` rows.

### How far out of distribution is it? Measured on real data: badly.

Scored the same 955k-event corpus both ways. This is the whole argument:

| mode | authors | score >= 0.99 | mean | median |
|---|---|---|---|---|
| `replies` | 1,818 | 41 (**2.3%**) | 0.036 | 0.000 |
| `roots` | 2,231 | 652 (**29.2%**) | 0.410 | 0.129 |

A 13x difference. At threshold 0.99 the root pass would ban **652 authors and
delete 63,284 events — including 393 accounts with 100+ followers and 53 with
over 1,000.**

Sampling those flagged root authors and reading their notes settles it. They
were: someone with 422 followers posting Bach and Rachmaninoff recordings; a
Spanish-speaking Bitcoin commentator; a jiu-jitsu podcaster; an account with 565
followers whose flagged post was "RIP Bernie Mac. Legend." Not one was a bot.

The reply pass on the same corpus is the control, and it is excellent — its top
scorers are a literal `🤖` URL-tracking-stripper bot with 14,325 replies, a
templated zap-solicitation bot repeating near-identical messages, and a
serialized cartoon poster. The model is not weak; it is simply being asked a
question it was never trained on.

A tag ablation on the labeled fixtures had predicted only a small shift (removing
`e`+`p` tags cost 1 false positive in 38). That was misleading, because it kept
reply *text*. The real shift is stylistic: ordinary feed posts — announcements,
media links, hashtags — look like promotional spam to a model that only ever
learned what a normal *reply* looks like.

**Conclusion: do not ban on `roots` or `all` with this model.** `ban_bots`
refuses non-reply modes unless `--allow-out-of-distribution` is passed, and that
flag should stay unused until there is a model actually trained on feed posts.

Root-note scoring is still useful for *ranking* — deprioritizing suspect content
in feeds is reversible in a way that deletion is not.

Because banning is author-level, the reply pass already deletes the root notes of
every bot it catches. That is where the space savings come from.

Authors with fewer than 3 notes are excluded: the model card calls out `<3` as
weak signal.

Still not covered: bots operating purely through kinds 0/3/6/7/10002, which
bypass the WoT gate entirely and post no kind-1 content to score.

## Deploying to production

See [DEPLOY.md](DEPLOY.md) for the full runbook: migration timing, the
block-then-delete ordering, throughput tuning, post-purge maintenance and undo.

## Setup

```bash
cd scripts/nspam
python3 -m venv .venv
.venv/bin/pip install -r requirements.txt
.venv/bin/python verify_parity.py      # must print PARITY OK
```

The model is downloaded from the Hugging Face Hub on first use and cached.
`scorer.py` pins `PINNED_REVISION` to a commit SHA rather than a branch — a
silent upstream retrain is exactly the failure mode that produces an
unexplainable ban wave. Override with `NSPAM_REVISION` only when deliberately
upgrading, and re-run the parity gate afterwards.

## The parity gate

`verify_parity.py` reproduces the reference implementation bit-for-bit:

- `hash_fixtures.jsonl` — token-level bucket and sign checks for both analyzers.
- `parity_fixtures.jsonl` — 50 real author bundles with reference scores.

Current status: **exact match, zero error on all 60 fixtures.**

It runs unconditionally inside `score_authors.py` and there is no flag to skip
it. A featurization bug is silent: it produces plausible scores, not errors, and
the resulting ban list would look entirely reasonable while being wrong.

Two details are easy to get backwards and are what the gate protects:

1. The **whole bundle is one document** — every note's text is joined with a
   space and vectorized once, not vectorized per note and summed.
2. The **char analyzer reads NFKC-only text with invisibles preserved**, while
   the **word analyzer reads fully normalized text** (invisibles stripped, URLs
   collapsed to scheme+host, casefolded, whitespace collapsed).

Feature layout, confirmed by arithmetic against the model's declared
`max_feature_idx=262166`:

| range | block |
|---|---|
| `[0, 131072)` | char_wb 3–5 grams, hashed |
| `[131072, 262144)` | word 1–2 grams, hashed |
| `[262144, 262161)` | 17 structural features, **mean-aggregated over the bundle** |
| `[262161, 262167)` | 6 group features |

## Phase 1 — one-time cleanup

Do the reply pass first — it is the in-distribution one, and because banning is
author-level it already removes those bots' root notes too. Then do a separate
root pass with its own threshold to catch the root-only bots.

```bash
# ── Pass 1: replies (in-distribution) ──
.venv/bin/python score_authors.py --dry-run --limit 5000     # smoke test
.venv/bin/python score_authors.py --execute

# Pick a threshold from the distribution — do not guess one up front.
.venv/bin/python review.py --scored-on replies hist

# Read every high-follower flagged account. These are the expensive mistakes.
.venv/bin/python review.py --scored-on replies flagged-vips --threshold 0.95 --min-followers 100

# Spot-check the bands straddling your threshold.
.venv/bin/python review.py --scored-on replies sample --min 0.90 --max 0.95 -n 20

# Confirm the ban set (dry run first).
.venv/bin/python review.py --scored-on replies promote --threshold 0.97 --max-bans 500
.venv/bin/python review.py --scored-on replies promote --threshold 0.97 --max-bans 500 --execute

# Ban and purge. Archives to JSONL before deleting.
cd ../.. && cargo run --release --bin ban_bots
cargo run --release --bin ban_bots -- --execute --max-bans 500 --archive /var/backups/nspam/wave1.jsonl

# ── Pass 2: root notes — SCORING ONLY, do not ban (see above) ──
cd scripts/nspam
.venv/bin/python score_authors.py --notes roots --execute
.venv/bin/python review.py --scored-on roots hist
.venv/bin/python review.py --scored-on roots sample --min 0.99 -n 20
# ban_bots refuses --scored-on roots without --allow-out-of-distribution.
# Leave that flag unused until there is a model trained on feed posts.
```

`review.py` defaults to `--scored-on replies`. Pass it explicitly when working
on another mode; `--scored-on any` pools them and is only ever for eyeballing
raw counts, never for picking a threshold.

Then, **in this order** — the ordering is load-bearing:

1. Flush Redis, including `DEL nostr:unique_pubkeys` (a HyperLogLog; elements
   cannot be removed, only a rebuild corrects it).
2. `REFRESH MATERIALIZED VIEW CONCURRENTLY profile_search;` — must be first,
   three analytics views join it.
3. Refresh the analytics views.
4. Restart the API so `ProfileSearchCache` / `WotCache` / `FollowerCache` reload.
5. `VACUUM (ANALYZE)` the touched tables. Plain `VACUUM`, not `FULL` — `FULL`
   takes an ACCESS EXCLUSIVE lock and needs as much free space as the table.

For the first wave, cap at a few hundred and hold for 48h before continuing. If
something is systematically wrong you want to find out at 500, not at 50,000.

## Seeing what got flagged

Terminal, with display names and npubs:

```bash
.venv/bin/python review.py --scored-on replies hist            # distribution + cost per threshold
.venv/bin/python review.py --scored-on replies sample --min 0.99 -n 20
.venv/bin/python review.py --scored-on replies flagged-vips --threshold 0.95 --min-followers 100
```

Or a browsable page — name, npub, score, the exact notes the model saw, and a
link through to njump for each account:

```bash
.venv/bin/python review.py --scored-on replies report --threshold 0.99 --out flagged.html
xdg-open flagged.html
```

Cards are chipped with the things that matter for a decision: follower count,
thin bundles (<10 notes, where most false positives live) and bridged accounts.
The page is written locally and deliberately not published — it contains real
pubkeys and note content.

For bridged accounts there is no scoring step; the dry run lists them directly:

```bash
cargo run --release --bin ban_bots -- --bridged --max-fraction 1.0
```

## Throughput — purges are sequential

`ban_bots` awaits each author's purge before starting the next, and the
in-service `purge_worker` is a single consumer doing the same. One high-volume
account holds up everything behind it.

Measured per 5,000-event batch:

| table | per batch |
|---|---|
| events | 693 ms |
| search_index | 199 ms |
| seen_events | 116 ms |
| event_refs | 93 ms |
| note_hashtags | 67 ms |
| zap_metadata | 59 ms |
| missing_events | 39 ms |
| **total** | **~1.27 s** |

That is ~4,000 events/sec, dominated by the `events` table's ~25 indexes. A bot
with 15,000 notes is three batches, about 4 seconds. Serialization is not the
constraint at this scale.

`--sleep` is, for wide sweeps. Purging all 50,962 bridged accounts is ~57s of
actual work, but at the default 100 ms between authors it spends 85 minutes
sleeping:

| `--sleep` | total |
|---|---|
| 100 (default) | ~86 min |
| 20 | ~18 min |
| 0 | ~1 min |

The pause exists to keep the live service responsive, so lower it deliberately
rather than by default — but for a large sweep of small accounts it is the only
number that matters.

Note `note_hashtags` has no index on `event_id` (migration 034 omits it on
purpose). It is not a problem here: the purge binds the id list as a query
parameter, which Postgres hashes, so the scan costs 67 ms rather than the
multiple seconds an inlined `ANY(ARRAY(SELECT …))` would.

## Undo

`ban_bots` writes every deleted event to JSONL before deleting. Restoring is a
tested command, not an improvised one:

```bash
.venv/bin/python restore_archive.py --file /var/backups/nspam/wave1.jsonl            # dry run
.venv/bin/python restore_archive.py --file wave1.jsonl --execute --unblock           # full undo
.venv/bin/python restore_archive.py --file wave1.jsonl --execute --unblock --pubkey <hex>
```

`--unblock` also lifts `blocked_pubkeys` and flips the score row to `cleared`;
without it the author stays blocked and ingestion keeps rejecting them.

Restore rebuilds `event_refs` and `is_reply` from the archived tags, and the
insert triggers rebuild `search_index` and `note_hashtags`. What does **not**
come back: `follows`/`follow_lists` (they return when the author's kind-3 is
re-ingested) and engagement counters the author contributed to *other* people's
notes.

### Retention — the archive does not expire on its own

Nothing deletes archives; restore capability lasts exactly as long as you keep
the file. Measured on this corpus, an event costs **5,698 bytes** inside
Postgres (across `events`, `search_index`, `note_hashtags`, `event_refs` — 70%
of it index overhead) versus **241 bytes** as zstd-compressed JSONL. So the
archive is ~24x smaller than what deleting reclaims, and it lives outside the
database: no indexes, no query cost, not in your DB backups.

Suggested policy: keep a wave's archive until it has been live for a week or two
without complaints, copy it off the database host, then delete it. Keeping them
forever is cheap but it is still unbounded growth.

Verified end to end on a real 955k-event corpus: purge 1,924 events → restore →
API serves the author's replies again with 1,673 `is_reply` and 1,674
`event_refs` correctly reconstructed.

## Local testing

```bash
docker compose up -d postgres redis
set -a; . scripts/nspam/local_env.sh; set +a
cargo run --bin nostr-api          # :8000 API, :8001 ws
curl -s localhost:8000/v1/stats
```

`local_env.sh` disables the crawler, relay discovery, negentropy and on-demand
fetch, so a local run never connects to real relays or pulls the live firehose
into the test database.

After any purge, the stats counters stay stale until flushed — `total_events` is
a monotonic Redis counter and `unique_pubkeys` is a HyperLogLog that cannot have
elements removed:

```bash
docker exec <redis> redis-cli DEL nostr:total_events nostr:unique_pubkeys nostr:events_by_kind
```

## Phase 2 — ongoing moderation

`moderate.sh`, from cron. Scores authors with new reply activity, auto-bans only
the extreme tail, and leaves everything else for human review.

```
0 3 * * * /opt/apps/nostr-api/scripts/nspam/moderate.sh
```

Tunables (environment): `AUTOBAN_THRESHOLD` (default 0.995), `MIN_REPLIES` (5),
`MAX_FOLLOWERS` (100), `MAX_AUTO_BANS` (200). Set `AUTOBAN_THRESHOLD=2` to
disable auto-banning and queue everything for review.

Incremental selection uses `author_spam_scores.newest_reply_at` as a watermark,
so a nightly run costs thousands of authors rather than millions.

## Why blocking must come before deleting

Deleting without blocking is **self-reversing**:

- Negentropy builds its local ID set from `SELECT id FROM events`, so deleted
  ids come back as `need_ids` on the next sync.
- `crawl_state` re-seeds from `follows` every 300s, using rows where the bot is
  the `followed_pubkey` — those belong to *other* users' contact lists and
  survive the purge.
- The `missing_events` drainer actively re-fetches deleted ids, and every
  inbound reaction/repost/zap aimed at a deleted note enqueues a new re-fetch.

`ban_bots` inserts into `blocked_pubkeys` before each author's data is deleted,
and guards were added to `sync_from_raw_follows` and `process_follow_list` so
neither re-seeds a blocked author.

## Guardrails

| Guardrail | Where |
|---|---|
| Allowlist (`spam_allowlist`) | candidate selection, `promote`, `ban_bots` |
| Follower exemption (default ≥100 spared) | `promote`, recorded as `decision='exempt'` |
| Reply-count floor (default ≥3) | candidate selection and re-checked at score time |
| `--max-bans` cap, required with `--execute` | `ban_bots` |
| Refuse if ban set > 5% of all authors | `ban_bots`, `promote` |
| Refuse if scores are stale (>7d) or absent | `ban_bots` |
| JSONL archive of every deleted event | `ban_bots`, verified complete before deleting |
| A rescore never un-decides a reviewed author | `db.py` upsert `CASE` |

The model card itself advises combining the score with mutes and the follow
graph "rather than hard-blocking on a single score" — the review gate, follower
exemption, and allowlist are how that advice is honored here.

## Files

| File | Purpose |
|---|---|
| `preprocess.py` | NFKC, invisible-char handling, URL normalization, casefold |
| `features.py` | vectorizers, 17 structural + 6 group features, matrix assembly |
| `scorer.py` | model download/load, LightGBM predict, isotonic calibration |
| `db.py` | candidate selection, LATERAL note fetch, score upserts |
| `verify_parity.py` | the parity gate |
| `score_authors.py` | batch scoring entry point |
| `review.py` | `hist` / `sample` / `flagged-vips` / `promote` / `decide` |
| `report.py` | HTML review page generator (used by `review.py report`) |
| `nip19.py` | npub encoding, so review output names accounts instead of hex |
| `restore_archive.py` | undo a purge from a `ban_bots` archive |
| `moderate.sh` | cron wrapper for ongoing moderation |
| `local_env.sh` | isolated local env for testing (no outbound relay traffic) |
