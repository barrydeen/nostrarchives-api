"""Review tooling for nspam scores — pick a threshold, then confirm bans.

Everything here is read-only except `decide` and `promote`.

Subcommands:
    hist          score distribution + what each candidate threshold would cost
    sample        random authors in a score band, with the notes that were scored
    flagged-vips  high-follower flagged accounts — read this list in full
    promote       bulk-set decision='confirmed' above a threshold, with guardrails
    decide        apply decisions from a CSV (pubkey,decision,reason)

Usage:
    python review.py hist
    python review.py sample --min 0.90 --max 0.95 -n 20
    python review.py flagged-vips --threshold 0.95 --min-followers 100
    python review.py promote --threshold 0.97 --max-bans 500
"""

from __future__ import annotations

import argparse
import csv
import getpass
import sys

import db
import report as report_mod
from nip19 import encode_npub

THRESHOLDS = [0.5, 0.6, 0.7, 0.8, 0.85, 0.9, 0.95, 0.98, 0.99, 0.995, 0.999]


def mode_clause(args, alias: str = "") -> str:
    """SQL fragment restricting to one scored_on mode.

    Defaults to a real filter: an unscoped write would flip both the reply and
    the root row for the same author, and an unscoped read would pool two
    incomparable distributions into one histogram.
    """
    mode = getattr(args, "scored_on", None) or "replies"
    if mode == "any":
        return ""
    col = f"{alias}.scored_on" if alias else "scored_on"
    return f" AND {col} = '{mode}'"


def cmd_hist(conn, args) -> int:
    with conn.cursor() as cur:
        cur.execute(
            "SELECT COUNT(*), AVG(score), percentile_cont(0.5) WITHIN GROUP (ORDER BY score) "
            f"FROM author_spam_scores WHERE true{mode_clause(args)}"
        )
        total, mean, median = cur.fetchone()
        if not total:
            print("no scores yet — run score_authors.py --execute first")
            return 1
        print(f"{total:,} scored authors   mean {mean:.4f}   median {median:.4f}\n")

        print("distribution (0.05 buckets):")
        cur.execute(
            """
            SELECT width_bucket(score, 0, 1, 20) AS b, COUNT(*)
            FROM author_spam_scores WHERE true{mode}
            GROUP BY b ORDER BY b
            """.format(mode=mode_clause(args))
        )
        rows = cur.fetchall()
        peak = max((r[1] for r in rows), default=1)
        for b, n in rows:
            lo = (b - 1) * 0.05
            bar = "█" * max(1, int(40 * n / peak))
            print(f"  {lo:.2f}-{lo + 0.05:.2f} {n:>9,} {bar}")

        # The decision-driver: how many accounts with a real audience would be
        # caught. Those are the false positives that actually cost you.
        print(f"\n{'threshold':>10} {'authors':>10} {'events':>12} {'>=100 flwr':>11} {'>=1k flwr':>10}")
        for t in THRESHOLDS:
            cur.execute(
                """
                SELECT COUNT(*),
                       COALESCE(SUM(total_replies), 0),
                       COUNT(*) FILTER (WHERE follower_count >= 100),
                       COUNT(*) FILTER (WHERE follower_count >= 1000)
                FROM author_spam_scores
                WHERE score >= %s AND decision = 'pending'""" + mode_clause(args) + """
                """,
                (t,),
            )
            n, ev, f100, f1k = cur.fetchone()
            print(f"{t:>10} {n:>10,} {ev:>12,} {f100:>11,} {f1k:>10,}")

        # The model card calls out <3 replies as weak signal; show the operator
        # the evidence rather than assuming a single threshold fits both groups.
        print(f"\nby bundle size (at score >= {args.threshold}):")
        cur.execute(
            """
            SELECT CASE WHEN n_replies_scored <= 2 THEN '1-2'
                        WHEN n_replies_scored <= 5 THEN '3-5' ELSE '6-10' END AS band,
                   COUNT(*), AVG(score)
            FROM author_spam_scores WHERE score >= %s""" + mode_clause(args) + """
            GROUP BY band ORDER BY band
            """,
            (args.threshold,),
        )
        for band, n, avg in cur.fetchall():
            print(f"  {band:>5} replies: {n:>9,} authors, mean score {avg:.4f}")
    return 0


def cmd_sample(conn, args) -> int:
    with conn.cursor() as cur:
        cur.execute(
            """
            SELECT pubkey, score, raw_score, follower_count, n_replies_scored,
                   total_replies, scored_on
            FROM author_spam_scores
            WHERE score >= %s AND score < %s AND decision = 'pending'""" + mode_clause(args) + """
            ORDER BY md5(pubkey || %s) LIMIT %s
            """,
            (args.min, args.max, args.seed, args.n),
        )
        rows = cur.fetchall()
    if not rows:
        print("no authors in that band")
        return 0

    profiles = db.fetch_profiles(conn, [r[0] for r in rows])
    print(f"{len(rows)} authors in [{args.min}, {args.max})  seed={args.seed}\n")
    for pk, score, raw, flwr, n_scored, total, mode in rows:
        prof = profiles.get(pk, {})
        print("=" * 78)
        print(f"{prof.get('name') or '(no display name)'}"
              + (f"  <{prof['nip05']}>" if prof.get("nip05") else ""))
        print(f"  {encode_npub(pk)}")
        print(f"  score {score:.4f} (raw {raw:.4f})  followers {flwr:,}  "
              f"{n_scored} {mode} scored / {total:,} total")
        # Show the same notes the model saw, not a different slice.
        bundles = db.fetch_bundles(conn, [pk], n_scored, mode)
        for note in bundles.get(pk, []):
            body = " ".join((note["content"] or "").split())
            print(f"    · {body[:300]}")
        print()
    return 0


def cmd_flagged_vips(conn, args) -> int:
    with conn.cursor() as cur:
        cur.execute(
            """
            SELECT pubkey, score, follower_count, n_replies_scored, total_replies, scored_on
            FROM author_spam_scores
            WHERE score >= %s AND follower_count >= %s AND decision = 'pending'""" + mode_clause(args) + """
            ORDER BY follower_count DESC LIMIT %s
            """,
            (args.threshold, args.min_followers, args.limit),
        )
        rows = cur.fetchall()

    if not rows:
        print("no flagged accounts above that follower count — good sign")
        return 0

    profiles = db.fetch_profiles(conn, [r[0] for r in rows])
    print(f"{len(rows)} flagged accounts with >= {args.min_followers} followers.")
    print("Read every one of these before banning. These are the expensive mistakes.\n")
    for pk, score, flwr, n_scored, total, mode in rows:
        prof = profiles.get(pk, {})
        print("=" * 78)
        print(f"{prof.get('name') or '(no display name)'}"
              + (f"  <{prof['nip05']}>" if prof.get("nip05") else ""))
        print(f"  {encode_npub(pk)}")
        print(f"  score {score:.4f}  followers {flwr:,}  {mode} {total:,}")
        for note in db.fetch_bundles(conn, [pk], n_scored, mode).get(pk, []):
            body = " ".join((note["content"] or "").split())
            print(f"    · {body[:300]}")
        print()
    return 0


def cmd_promote(conn, args) -> int:
    """Bulk-confirm above a threshold, honoring every guardrail."""
    with conn.cursor() as cur:
        cur.execute(
            """
            SELECT COUNT(*) FROM author_spam_scores s
            WHERE s.score >= %s
              AND s.decision = 'pending'
              AND s.n_replies_scored >= %s
              AND s.follower_count < %s
              AND NOT EXISTS (SELECT 1 FROM spam_allowlist a WHERE a.pubkey = s.pubkey)""" + mode_clause(args, "s") + """
            """,
            (args.threshold, args.min_replies, args.max_followers),
        )
        eligible = cur.fetchone()[0]

        cur.execute("SELECT COUNT(DISTINCT pubkey) FROM events")
        total_authors = cur.fetchone()[0]

    print(f"threshold {args.threshold}, min replies {args.min_replies}, "
          f"follower exemption >= {args.max_followers}")
    print(f"eligible: {eligible:,} authors")
    print(f"cap:      {args.max_bans:,}")

    # A run that wants to ban a large fraction of the user base is a bug, not a
    # spam wave. Refuse rather than proceed.
    if total_authors and eligible > total_authors * args.max_fraction:
        pct = 100.0 * eligible / total_authors
        print(f"\nREFUSING: {pct:.1f}% of all {total_authors:,} authors exceeds the "
              f"{100 * args.max_fraction:.0f}% safety limit.")
        return 1

    to_promote = min(eligible, args.max_bans)
    if not to_promote:
        print("nothing to promote")
        return 0

    if not args.execute:
        print(f"\nDRY RUN — would confirm {to_promote:,} authors. Re-run with --execute.")
        return 0

    typed = input(f"\nType 'confirm {to_promote}' to proceed: ").strip()
    if typed != f"confirm {to_promote}":
        print("aborted")
        return 1

    reason = args.reason or f"auto: score >= {args.threshold} (model {args.model_version})"
    with conn.cursor() as cur:
        cur.execute(
            """
            UPDATE author_spam_scores SET
                decision = 'confirmed', decided_at = NOW(),
                decided_by = %s, decision_reason = %s
            WHERE (pubkey, scored_on) IN (
                SELECT s.pubkey, s.scored_on FROM author_spam_scores s
                WHERE s.score >= %s AND s.decision = 'pending'
                  AND s.n_replies_scored >= %s AND s.follower_count < %s
                  AND NOT EXISTS (SELECT 1 FROM spam_allowlist a WHERE a.pubkey = s.pubkey)""" + mode_clause(args, "s") + """
                ORDER BY s.score DESC LIMIT %s
            )
            """,
            (getpass.getuser(), reason, args.threshold, args.min_replies,
             args.max_followers, to_promote),
        )
        promoted = cur.rowcount

        # Record why high-follower accounts were spared, rather than skipping
        # them silently, so review can see the guardrail firing.
        cur.execute(
            """
            UPDATE author_spam_scores SET
                decision = 'exempt', decided_at = NOW(), decided_by = %s,
                decision_reason = 'guardrail: follower_count >= ' || %s
            WHERE score >= %s AND decision = 'pending' AND follower_count >= %s""" + mode_clause(args) + """
            """,
            (getpass.getuser(), str(args.max_followers), args.threshold, args.max_followers),
        )
        exempted = cur.rowcount
    conn.commit()
    print(f"confirmed {promoted:,} authors, marked {exempted:,} exempt")
    print("Next: run `cargo run --release --bin ban_bots` (dry run first).")
    return 0


def cmd_decide(conn, args) -> int:
    with open(args.file, newline="") as fh:
        rows = [r for r in csv.DictReader(fh)]
    valid = {"pending", "confirmed", "cleared", "exempt"}
    bad = [r for r in rows if r.get("decision") not in valid]
    if bad:
        print(f"{len(bad)} rows have an invalid decision (allowed: {sorted(valid)})")
        return 1

    with conn.cursor() as cur:
        cur.executemany(
            """
            UPDATE author_spam_scores SET
                decision = %s, decided_at = NOW(), decided_by = %s, decision_reason = %s
            WHERE pubkey = %s AND scored_on = %s
            """,
            [(r["decision"], getpass.getuser(), r.get("reason"), r["pubkey"],
              r.get("scored_on") or args.scored_on) for r in rows],
        )
    conn.commit()
    print(f"applied {len(rows)} decisions")
    return 0


def cmd_report(conn, args) -> int:
    """Write a browsable HTML page of the flagged set."""
    with conn.cursor() as cur:
        cur.execute(
            """
            SELECT pubkey, score, follower_count, n_replies_scored, total_replies, scored_on
            FROM author_spam_scores
            WHERE score >= %s AND decision = 'pending'""" + mode_clause(args) + """
            ORDER BY score DESC, total_replies DESC LIMIT %s
            """,
            (args.threshold, args.limit),
        )
        rows = cur.fetchall()

    if not rows:
        print("nothing at or above that threshold — run score_authors.py first")
        return 1

    enriched = []
    for pk, score, flwr, n_scored, total, mode in rows:
        notes = [n["content"] for n in db.fetch_bundles(conn, [pk], n_scored, mode).get(pk, [])]
        enriched.append({
            "pubkey": pk, "score": float(score), "follower_count": int(flwr),
            "n_scored": int(n_scored), "total": int(total), "mode": mode,
            "notes": notes,
        })

    html_doc = report_mod.build(
        conn, enriched,
        title=f"Flagged authors — score >= {args.threshold}",
        note=f"Scored on {args.scored_on} notes. Sorted by score, then volume.",
    )
    with open(args.out, "w", encoding="utf-8") as fh:
        fh.write(html_doc)
    print(f"wrote {args.out} ({len(enriched)} authors)")
    print(f"open it with:  xdg-open {args.out}")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--dsn", default=None)
    ap.add_argument("--scored-on", choices=["replies", "roots", "all", "any"],
                    default="replies",
                    help="which score mode to operate on. Defaults to 'replies' "
                         "because reply-derived and root-derived scores are NOT "
                         "comparable and must not share a threshold. 'any' pools "
                         "them — only meaningful for eyeballing raw counts, never "
                         "for picking a threshold")
    sub = ap.add_subparsers(dest="cmd", required=True)

    p = sub.add_parser("hist"); p.add_argument("--threshold", type=float, default=0.9)
    p.set_defaults(fn=cmd_hist)

    p = sub.add_parser("sample")
    p.add_argument("--min", type=float, default=0.9)
    p.add_argument("--max", type=float, default=1.01)
    p.add_argument("-n", type=int, default=20)
    p.add_argument("--seed", default="nspam")
    p.set_defaults(fn=cmd_sample)

    p = sub.add_parser("flagged-vips")
    p.add_argument("--threshold", type=float, default=0.9)
    p.add_argument("--min-followers", type=int, default=100)
    p.add_argument("--limit", type=int, default=200)
    p.set_defaults(fn=cmd_flagged_vips)

    p = sub.add_parser("promote")
    p.add_argument("--threshold", type=float, required=True)
    p.add_argument("--max-bans", type=int, required=True)
    p.add_argument("--min-replies", type=int, default=10,
                   help="require a full bundle. Measured: 7 of 18 otherwise-bannable "
                        "authors had 3-6 notes, and they were where nearly every "
                        "false positive lived (European Commission, Framework, "
                        "non-English speakers). The model card warns about thin "
                        "bundles; this is that warning applied.")
    p.add_argument("--max-followers", type=int, default=100,
                   help="accounts at or above this are exempted, not banned")
    p.add_argument("--max-fraction", type=float, default=0.05,
                   help="refuse if eligible exceeds this fraction of all authors")
    p.add_argument("--model-version", default="v2.2")
    p.add_argument("--reason", default=None)
    p.add_argument("--execute", action="store_true")
    p.set_defaults(fn=cmd_promote)

    p = sub.add_parser("report")
    p.add_argument("--threshold", type=float, default=0.9)
    p.add_argument("--limit", type=int, default=300)
    p.add_argument("--out", default="flagged.html")
    p.set_defaults(fn=cmd_report)

    p = sub.add_parser("decide"); p.add_argument("--file", required=True)
    p.set_defaults(fn=cmd_decide)

    args = ap.parse_args()
    conn = db.connect(args.dsn)
    try:
        return args.fn(conn, args)
    finally:
        conn.close()


if __name__ == "__main__":
    sys.exit(main())
