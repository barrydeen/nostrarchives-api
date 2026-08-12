"""Restore events from a ban_bots archive back into the database.

The archive is the only undo for a purge, so restoring must be a tested command,
not an improvised one — hand-rolling COPY escaping in an emergency is how the
backup stops being a backup.

Restoring does NOT unblock the author. `blocked_pubkeys` still holds them, so
ingestion keeps rejecting new events and the crawler stays away. Use
`--unblock` to lift that too, which is what you want when reversing a mistake.

Note this restores the `events` rows. Derived rows are rebuilt by their own
machinery: `search_index` and `note_hashtags` come back via the AFTER INSERT
triggers; `follows` and `follow_lists` return when the author's kind-3 is
re-ingested. Engagement counters on other people's notes are not restored.

Usage:
    python restore_archive.py --file wave1.jsonl                    # dry run
    python restore_archive.py --file wave1.jsonl --execute
    python restore_archive.py --file wave1.jsonl --execute --unblock
    python restore_archive.py --file wave1.jsonl --execute --pubkey <hex>
"""

from __future__ import annotations

import argparse
import json
import sys

import db

INSERT_SQL = """
INSERT INTO events (id, pubkey, created_at, kind, content, sig, tags, raw,
                    is_reply, is_machine_note, reaction_count, repost_count,
                    reply_count, zap_count, zap_amount_msats)
VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s)
ON CONFLICT (id) DO NOTHING
"""

REF_SQL = """
INSERT INTO event_refs (source_event_id, target_event_id, ref_type, relay_hint, created_at)
VALUES (%s, %s, %s, %s, %s)
ON CONFLICT DO NOTHING
"""


def classify_refs(event: dict) -> list[tuple[str, str, str | None]]:
    """Rebuild an event's e-tag references.

    Mirrors `insert_refs` in src/db/repository.rs: an explicit marker wins, and
    unmarked tags fall back to the legacy positional rule (a lone e-tag is a
    reply; otherwise first is root, last is reply, the rest are mentions).

    Returns (target_id, ref_type, relay_hint) triples.
    """
    e_tags = [t for t in (event.get("tags") or []) if len(t) >= 2 and t[0] == "e"]
    out = []
    for i, tag in enumerate(e_tags):
        marker = tag[3] if len(tag) > 3 else None
        if marker in ("root", "reply", "mention"):
            ref_type = marker
        elif marker is None:
            if len(e_tags) == 1:
                ref_type = "reply"
            elif i == 0:
                ref_type = "root"
            elif i == len(e_tags) - 1:
                ref_type = "reply"
            else:
                ref_type = "mention"
        else:
            ref_type = "mention"
        hint = tag[2] if len(tag) > 2 and tag[2] else None
        out.append((tag[1], ref_type, hint))
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--file", required=True, help="JSONL archive written by ban_bots")
    ap.add_argument("--dsn", default=None)
    ap.add_argument("--pubkey", default=None, help="restore only this author")
    ap.add_argument("--batch", type=int, default=1000)
    ap.add_argument("--unblock", action="store_true",
                    help="also remove the authors from blocked_pubkeys")
    ap.add_argument("--execute", action="store_true", help="default is a dry run")
    args = ap.parse_args()

    rows, events, bad = [], [], 0
    with open(args.file) as fh:
        for n, line in enumerate(fh, 1):
            line = line.strip()
            if not line:
                continue
            try:
                e = json.loads(line)
            except json.JSONDecodeError:
                bad += 1
                print(f"  line {n}: not valid JSON, skipping")
                continue
            if args.pubkey and e.get("pubkey") != args.pubkey:
                continue
            events.append(e)
            rows.append((
                e["id"], e["pubkey"], e["created_at"], e["kind"],
                e.get("content") or "", e.get("sig") or "",
                json.dumps(e.get("tags") or []),
                json.dumps(e.get("raw") or {}),
                # Older archives predate these fields; fall back to defaults so
                # a legacy file still restores rather than erroring.
                bool(e.get("is_reply", False)),
                bool(e.get("is_machine_note", False)),
                int(e.get("reaction_count", 0)),
                int(e.get("repost_count", 0)),
                int(e.get("reply_count", 0)),
                int(e.get("zap_count", 0)),
                int(e.get("zap_amount_msats", 0)),
            ))

    authors = {r[1] for r in rows}
    print(f"{len(rows):,} events across {len(authors):,} authors in {args.file}")
    if bad:
        print(f"WARNING: {bad} unparseable lines skipped")
    if not rows:
        print("nothing to restore")
        return 0
    if not args.execute:
        print("\nDRY RUN — nothing written. Re-run with --execute.")
        return 0

    conn = db.connect(args.dsn)
    try:
        restored = 0
        with conn.cursor() as cur:
            for i in range(0, len(rows), args.batch):
                chunk = rows[i:i + args.batch]
                cur.executemany(INSERT_SQL, chunk)
                restored += len(chunk)
                print(f"  restored {restored:,}/{len(rows):,}", flush=True)

            # Rebuild event_refs from the tags. Without these, replies are
            # orphaned from their threads even though the rows are back.
            refs = []
            for e in events:
                for target, ref_type, hint in classify_refs(e):
                    refs.append((e["id"], target, ref_type, hint, e["created_at"]))
            if refs:
                cur.executemany(REF_SQL, refs)
                print(f"rebuilt {len(refs):,} event_refs")

            # is_reply is derived, not stored in older archives — recompute it
            # from the refs we just wrote so both paths agree.
            cur.execute(
                """
                UPDATE events e SET is_reply = true
                WHERE e.pubkey = ANY(%s::text[]) AND e.kind = 1 AND NOT e.is_reply
                  AND EXISTS (SELECT 1 FROM event_refs r
                              WHERE r.source_event_id = e.id
                                AND r.ref_type IN ('reply','root'))
                """,
                (list(authors),),
            )
            if args.unblock:
                cur.execute(
                    "DELETE FROM blocked_pubkeys WHERE pubkey = ANY(%s::text[])",
                    (list(authors),),
                )
                print(f"unblocked {cur.rowcount} authors")
                cur.execute(
                    """
                    UPDATE author_spam_scores SET decision = 'cleared',
                        decided_at = NOW(), decision_reason = 'restored from archive'
                    WHERE pubkey = ANY(%s::text[]) AND decision = 'purged'
                    """,
                    (list(authors),),
                )
        conn.commit()
        print(f"\nrestored {restored:,} events")
        print("search_index and note_hashtags rebuild via triggers; event_refs and "
              "is_reply were rebuilt from tags. follows/follow_lists return when the "
              "author's kind-3 is re-ingested. Engagement counters on OTHER people's "
              "notes are not restored.")
        return 0
    finally:
        conn.close()


if __name__ == "__main__":
    sys.exit(main())
