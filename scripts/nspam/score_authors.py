"""Batch-score Nostr authors for reply spam with the nspam classifier.

Reads the last N reply notes for every candidate author, scores each author as
one bundle, and upserts the result into ``author_spam_scores``. Writes nothing
unless ``--execute`` is passed.

The parity gate runs unconditionally at startup. There is no flag to skip it —
a featurization bug is silent, and the resulting ban list would look completely
plausible while being wrong.

Usage:
    python score_authors.py --dry-run --limit 5000        # smoke test
    python score_authors.py --execute                     # full run
    python score_authors.py --execute --incremental       # ongoing moderation

Env: DATABASE_URL, NSPAM_REVISION (optional model pin override)
"""

from __future__ import annotations

import argparse
import sys
import time

import numpy as np

import db
from scorer import Nspam
from verify_parity import check_hashes, check_parity


def parse_args() -> argparse.Namespace:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--dsn", default=None, help="Postgres DSN (default: $DATABASE_URL)")
    ap.add_argument("--chunk-size", type=int, default=2000, help="authors per DB round trip")
    ap.add_argument("--notes-per-author", type=int, default=10, help="model accepts 1-10")
    ap.add_argument("--min-replies", type=int, default=3, dest="min_notes",
                    help="model card: <3 notes is weak signal")
    ap.add_argument("--notes", choices=["replies", "roots", "all"], default="replies",
                    help="which kind-1 notes feed the model. 'replies' matches "
                         "training; 'roots'/'all' are out-of-distribution and need "
                         "their own threshold, calibrated before banning")
    ap.add_argument("--limit", type=int, default=None, help="stop after N authors")
    ap.add_argument("--sleep-ms", type=int, default=50, help="pause between chunks")
    ap.add_argument("--incremental", action="store_true",
                    help="only authors with new replies since their last score")
    ap.add_argument("--reuse-candidates", action="store_true",
                    help="skip rebuilding spam_candidates (resume a run)")
    ap.add_argument("--execute", action="store_true", help="write scores (default: dry run)")
    ap.add_argument("--model-dir", default=None)
    return ap.parse_args()


def main() -> int:
    args = parse_args()

    if args.notes_per_author < 1 or args.notes_per_author > 10:
        print("--notes-per-author must be within the model's trained range of 1-10")
        return 2

    print("Loading model…")
    model = Nspam.load(args.model_dir)
    print(f"model {model.model_version}, {model.total_features} features\n")

    # ── Parity gate: non-negotiable, runs before we open a write path ──
    if not check_hashes(model):
        print("\nHASH PARITY FAILED — refusing to score.")
        return 1
    if not check_parity(model, 1e-6):
        print("\nPARITY FAILED — refusing to score.")
        return 1
    print("parity gate passed\n")

    conn = db.connect(args.dsn)
    mode = "EXECUTE" if args.execute else "DRY RUN"
    print(f"connected — {mode}, scoring on {args.notes}\n")
    if args.notes != "replies":
        print(
            "NOTE: the model is trained on replies only. Scores produced from "
            f"'{args.notes}' are out-of-distribution — calibrate a separate "
            "threshold with review.py before banning on them.\n"
        )

    try:
        if args.reuse_candidates:
            with conn.cursor() as cur:
                cur.execute("SELECT COUNT(*) FROM spam_candidates")
                n_candidates = cur.fetchone()[0]
            print(f"reusing existing spam_candidates: {n_candidates:,} authors")
        else:
            print("building candidate set…")
            t0 = time.time()
            n_candidates = db.build_candidates(conn, args.min_notes, args.notes)
            conn.commit()
            print(f"{n_candidates:,} candidate authors ({time.time() - t0:.1f}s)")

        if args.incremental:
            n_candidates = db.restrict_to_incremental(conn, scored_on=args.notes)
            conn.commit()
            print(f"incremental: {n_candidates:,} authors with new replies")

        if n_candidates == 0:
            print("nothing to score")
            return 0

        scored = 0
        skipped_thin = 0
        all_scores: list[float] = []
        t_start = time.time()

        for chunk in db.iter_candidate_chunks(conn, args.chunk_size, args.limit):
            pubkeys = [p for p, _ in chunk]
            reply_counts = dict(chunk)

            bundles_by_pk = db.fetch_bundles(
                conn, pubkeys, args.notes_per_author, args.notes
            )
            followers = db.fetch_follower_counts(conn, pubkeys)

            ordered_pks, bundles = [], []
            for pk in pubkeys:
                notes = bundles_by_pk.get(pk, [])
                # Re-check the floor: rows can vanish between candidate build
                # and fetch, and a thin bundle is exactly where the model is weak.
                if len(notes) < args.min_notes:
                    skipped_thin += 1
                    continue
                ordered_pks.append(pk)
                bundles.append(notes)

            if not bundles:
                continue

            raw, cal = model.score(bundles)
            all_scores.extend(cal.tolist())

            rows = [
                (
                    pk,
                    float(cal[i]),
                    float(raw[i]),
                    len(bundles[i]),
                    max((n["created_at"] or 0) for n in bundles[i]),
                    reply_counts.get(pk, 0),
                    followers.get(pk, 0),
                    model.model_version,
                    args.notes,
                )
                for i, pk in enumerate(ordered_pks)
            ]

            if args.execute:
                db.write_scores(conn, rows)

            scored += len(rows)
            rate = scored / max(time.time() - t_start, 1e-6)
            print(
                f"  scored {scored:,}/{n_candidates:,} "
                f"({rate:.0f}/s, {len(bundles)} in chunk)",
                flush=True,
            )

            if args.sleep_ms:
                time.sleep(args.sleep_ms / 1000.0)

        print(f"\nscored {scored:,} authors in {time.time() - t_start:.1f}s")
        if skipped_thin:
            print(f"skipped {skipped_thin:,} authors with <{args.min_notes} notes")

        if all_scores:
            a = np.array(all_scores)
            print("\nscore distribution:")
            for lo in (0.5, 0.8, 0.9, 0.95, 0.99):
                n = int((a >= lo).sum())
                print(f"  >= {lo:<5} {n:>10,}  ({100.0 * n / len(a):5.2f}%)")
            print(f"  median {np.median(a):.4f}   mean {a.mean():.4f}")

        if not args.execute:
            print("\nDRY RUN — nothing written. Re-run with --execute to persist.")
        return 0
    finally:
        conn.close()


if __name__ == "__main__":
    sys.exit(main())
