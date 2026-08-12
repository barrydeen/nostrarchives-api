"""Database access for the nspam scorer.

Read-only for scoring; the only writes are upserts into ``author_spam_scores``.
Deliberately single-connection: this is a batch job and the Rust service needs
the connection headroom.
"""

from __future__ import annotations

import os
from typing import Any, Iterator, Sequence

import psycopg
from psycopg.rows import dict_row

# Marks the connection in pg_stat_activity so it can be identified and killed.
APP_NAME = "nspam_scorer"
STATEMENT_TIMEOUT = "120s"


def connect(dsn: str | None = None) -> psycopg.Connection:
    dsn = dsn or os.environ.get("DATABASE_URL")
    if not dsn:
        raise SystemExit("DATABASE_URL not set (and no --dsn given)")
    conn = psycopg.connect(dsn, application_name=APP_NAME, autocommit=False)
    with conn.cursor() as cur:
        cur.execute(f"SET statement_timeout = '{STATEMENT_TIMEOUT}'")
    conn.commit()
    return conn


# Which kind-1 notes feed the model.
#
# 'replies' matches how the model was trained. 'roots' and 'all' are
# out-of-distribution — the model never saw feed posts — so they need their own
# threshold, calibrated on sampled output before anything is banned.
#
# Each predicate is covered by a partial index from migration 013:
#   (pubkey, created_at DESC) WHERE kind = 1 AND is_reply
#   (pubkey, created_at DESC) WHERE kind = 1 AND NOT is_reply
NOTE_FILTERS = {
    "replies": "kind = 1 AND is_reply",
    "roots": "kind = 1 AND NOT is_reply",
    "all": "kind = 1",
}


def build_candidates(
    conn: psycopg.Connection, min_notes: int, scored_on: str = "replies"
) -> int:
    """Materialize the candidate author set into an UNLOGGED table.

    Runs off the partial reply/root indexes (migration 013), so it never touches
    the events heap.

    Excludes authors who are already blocked, allowlisted, or whose score has
    already been decided. Authors still 'pending' from a previous run ARE
    re-scored, since their note window has moved.
    """
    where = NOTE_FILTERS[scored_on]
    with conn.cursor() as cur:
        cur.execute("DROP TABLE IF EXISTS spam_candidates")
        cur.execute(
            f"""
            CREATE UNLOGGED TABLE spam_candidates AS
            SELECT pubkey, COUNT(*)::int AS reply_count
            FROM events
            WHERE {where} AND NOT is_machine_note
            GROUP BY pubkey
            HAVING COUNT(*) >= %s
            """,
            (min_notes,),
        )
        cur.execute("ALTER TABLE spam_candidates ADD PRIMARY KEY (pubkey)")
        cur.execute(
            "DELETE FROM spam_candidates c USING blocked_pubkeys b WHERE b.pubkey = c.pubkey"
        )
        cur.execute(
            "DELETE FROM spam_candidates c USING spam_allowlist a WHERE a.pubkey = c.pubkey"
        )
        cur.execute(
            """
            DELETE FROM spam_candidates c USING author_spam_scores s
            WHERE s.pubkey = c.pubkey AND s.scored_on = %s AND s.decision <> 'pending'
            """,
            (scored_on,),
        )
        cur.execute("ANALYZE spam_candidates")
        cur.execute("SELECT COUNT(*) FROM spam_candidates")
        return cur.fetchone()[0]


def restrict_to_incremental(
    conn: psycopg.Connection, window_days: int = 30, scored_on: str = "replies"
) -> int:
    """Narrow spam_candidates to authors worth re-scoring.

    Keeps: never-scored authors, and authors whose newest reply is newer than
    the watermark recorded at their last score. Drops everyone else, so a daily
    run costs thousands of authors rather than millions.

    Authors dormant longer than `window_days` are, by definition, not currently
    spamming; bounding the scan to that window keeps it on the tail of the
    partial reply index instead of walking all of history.
    """
    where = NOTE_FILTERS[scored_on]
    with conn.cursor() as cur:
        cur.execute(
            f"""
            CREATE TEMP TABLE incr_recent ON COMMIT DROP AS
            SELECT e.pubkey, MAX(e.created_at) AS newest
            FROM events e
            WHERE {where} AND NOT e.is_machine_note
              AND e.created_at > EXTRACT(EPOCH FROM NOW() - make_interval(days => %s))::bigint
            GROUP BY e.pubkey
            """,
            (window_days,),
        )
        cur.execute("CREATE INDEX ON incr_recent (pubkey)")
        cur.execute("ANALYZE incr_recent")

        # Drop candidates with no recent reply activity at all.
        cur.execute(
            """
            DELETE FROM spam_candidates c
            WHERE NOT EXISTS (SELECT 1 FROM incr_recent r WHERE r.pubkey = c.pubkey)
            """
        )
        # Drop candidates whose newest reply we have already seen.
        cur.execute(
            """
            DELETE FROM spam_candidates c
            USING author_spam_scores s, incr_recent r
            WHERE s.pubkey = c.pubkey AND r.pubkey = c.pubkey
              AND s.scored_on = %s AND s.newest_reply_at >= r.newest
            """,
            (scored_on,),
        )
        cur.execute("SELECT COUNT(*) FROM spam_candidates")
        return cur.fetchone()[0]


def iter_candidate_chunks(
    conn: psycopg.Connection, chunk_size: int, limit: int | None = None
) -> Iterator[list[tuple[str, int]]]:
    """Keyset-paginate the candidate table so a resumed run is O(1) to seek."""
    last = ""
    yielded = 0
    while True:
        with conn.cursor() as cur:
            cur.execute(
                """
                SELECT pubkey, reply_count FROM spam_candidates
                WHERE pubkey > %s ORDER BY pubkey LIMIT %s
                """,
                (last, chunk_size),
            )
            rows = cur.fetchall()
        if not rows:
            return
        last = rows[-1][0]
        if limit is not None and yielded + len(rows) > limit:
            rows = rows[: limit - yielded]
        yield rows
        yielded += len(rows)
        if limit is not None and yielded >= limit:
            return


def fetch_bundles(
    conn: psycopg.Connection,
    pubkeys: Sequence[str],
    notes_per_author: int,
    scored_on: str = "replies",
) -> dict[str, list[dict[str, Any]]]:
    """Last N notes for each pubkey, via a LATERAL fan-out.

    LATERAL rather than a window function: a window function has to read every
    note row of every candidate to discard all but the newest N. This does
    exactly N index reads per author, the same access path profile_replies uses.
    """
    where = NOTE_FILTERS[scored_on]
    with conn.cursor(row_factory=dict_row) as cur:
        cur.execute(
            f"""
            SELECT c.pubkey, r.id, r.content, r.tags, r.created_at
            FROM unnest(%s::text[]) AS c(pubkey)
            CROSS JOIN LATERAL (
                SELECT e.id, e.content, e.tags, e.created_at
                FROM events e
                WHERE e.pubkey = c.pubkey AND {where} AND NOT e.is_machine_note
                ORDER BY e.created_at DESC
                LIMIT %s
            ) r
            """,
            (list(pubkeys), notes_per_author),
        )
        out: dict[str, list[dict[str, Any]]] = {}
        for row in cur:
            out.setdefault(row["pubkey"], []).append(
                {
                    "id": row["id"],
                    "content": row["content"],
                    "tags": row["tags"],
                    "created_at": row["created_at"],
                }
            )
    return out


def fetch_profiles(
    conn: psycopg.Connection, pubkeys: Sequence[str]
) -> dict[str, dict[str, str]]:
    """Latest kind-0 metadata per pubkey: name, nip05, picture.

    Reviewing a hex pubkey is impossible; a display name and nip05 are what let
    an operator recognise "oh, that's the European Commission bridge" at a
    glance. Malformed kind-0 content is common in the wild, so parse defensively.
    """
    import json

    if not pubkeys:
        return {}
    with conn.cursor() as cur:
        cur.execute(
            """
            SELECT DISTINCT ON (pubkey) pubkey, content
            FROM events
            WHERE kind = 0 AND pubkey = ANY(%s::text[])
            ORDER BY pubkey, created_at DESC
            """,
            (list(pubkeys),),
        )
        out: dict[str, dict[str, str]] = {}
        for pk, content in cur.fetchall():
            meta: dict[str, str] = {}
            try:
                m = json.loads(content or "{}")
                if isinstance(m, dict):
                    meta = {
                        "name": (m.get("display_name") or m.get("displayName")
                                 or m.get("name") or "").strip(),
                        "nip05": (m.get("nip05") or "").strip(),
                        "about": (m.get("about") or "").strip()[:200],
                    }
            except (ValueError, TypeError):
                pass
            out[pk] = meta
    return out


def fetch_follower_counts(
    conn: psycopg.Connection, pubkeys: Sequence[str]
) -> dict[str, int]:
    """Follower counts from profile_search. Used for review and as a ban guardrail."""
    with conn.cursor() as cur:
        cur.execute(
            """
            SELECT ps.pubkey, COALESCE(ps.follower_count, 0)
            FROM profile_search ps
            WHERE ps.pubkey = ANY(%s::text[])
            """,
            (list(pubkeys),),
        )
        return {r[0]: int(r[1]) for r in cur.fetchall()}


UPSERT_SQL = """
INSERT INTO author_spam_scores
    (pubkey, score, raw_score, n_replies_scored, newest_reply_at,
     total_replies, follower_count, model_version, scored_on, scored_at, decision)
VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s, NOW(), 'pending')
ON CONFLICT (pubkey, scored_on) DO UPDATE SET
    score            = EXCLUDED.score,
    raw_score        = EXCLUDED.raw_score,
    n_replies_scored = EXCLUDED.n_replies_scored,
    newest_reply_at  = EXCLUDED.newest_reply_at,
    total_replies    = EXCLUDED.total_replies,
    follower_count   = EXCLUDED.follower_count,
    model_version    = EXCLUDED.model_version,
    scored_on        = EXCLUDED.scored_on,
    scored_at        = NOW(),
    -- Never silently un-decide a reviewed author: a rescore must not flip a
    -- 'cleared' account back to 'pending' and re-expose it to a bulk ban.
    decision = CASE WHEN author_spam_scores.decision = 'pending'
                    THEN 'pending' ELSE author_spam_scores.decision END
"""

HISTORY_SQL = """
INSERT INTO author_spam_score_history
    (pubkey, score, raw_score, n_replies_scored, model_version)
VALUES (%s, %s, %s, %s, %s)
"""


def write_scores(conn: psycopg.Connection, rows: Sequence[tuple]) -> None:
    """Upsert a chunk of scores plus their append-only history rows."""
    if not rows:
        return
    with conn.cursor() as cur:
        cur.executemany(UPSERT_SQL, rows)
        cur.executemany(HISTORY_SQL, [(r[0], r[1], r[2], r[3], r[7]) for r in rows])
    conn.commit()
