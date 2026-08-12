"""Parity gate for the nspam featurization.

This must pass before any scoring run touches the database. Everything
downstream — the ban list, the purge — is worthless if featurization is wrong,
and a featurization bug is silent: it produces plausible scores, not errors.

Two levels, checked in order so a hashing bug can't hide behind an assembly bug:

  1. hash_fixtures.jsonl  — token-level bucket/sign checks for both analyzers.
  2. parity_fixtures.jsonl — full pipeline against reference scores.

Note on level 1: the fixture file truncates each char_wb bucket list at 32
entries, so for lists of exactly that length we assert prefix equality rather
than set equality. Word lists are never truncated in practice but the same rule
is applied for consistency.

Usage:  python verify_parity.py [--tolerance 1e-6]
Exit code 0 on success, 1 on any mismatch.
"""

from __future__ import annotations

import argparse
import json
import sys

import numpy as np

from features import build_vectorizers
from scorer import Nspam

TRUNCATION_LEN = 32


def _buckets(vec, text: str) -> list[tuple[int, float]]:
    m = vec.transform([text]).tocoo()
    return sorted((int(i), float(d)) for i, d in zip(m.col, m.data))


def check_hashes(model: Nspam) -> bool:
    path = model.model_dir / "hash_fixtures.jsonl"
    rows = [json.loads(line) for line in path.read_text().splitlines() if line.strip()]
    char_vec, word_vec = build_vectorizers(model.cfg)

    failures = 0
    for row in rows:
        token = row["token"]
        for key, vec in (("word_buckets", word_vec), ("char_wb_buckets", char_vec)):
            expected = sorted((b["index"], b["value"]) for b in row[key])
            got = _buckets(vec, token)
            # The fixture caps these lists at 32 entries.
            ok = (
                expected == got[: len(expected)]
                if len(expected) == TRUNCATION_LEN
                else expected == got
            )
            if not ok:
                failures += 1
                print(f"  FAIL {key} for {token!r}")
                print(f"    expected {expected[:6]}{' ...' if len(expected) > 6 else ''}")
                print(f"    got      {got[:6]}{' ...' if len(got) > 6 else ''}")

    print(f"hash fixtures: {len(rows)} tokens, {failures} failures")
    return failures == 0


def check_parity(model: Nspam, tolerance: float) -> bool:
    path = model.model_dir / "parity_fixtures.jsonl"
    rows = [json.loads(line) for line in path.read_text().splitlines() if line.strip()]

    bundles = [r["notes"] for r in rows]
    raw, cal = model.score(bundles)

    exp_raw = np.array([r["expected_raw_score"] for r in rows], dtype=np.float64)
    exp_cal = np.array([r["expected_calibrated_score"] for r in rows], dtype=np.float64)

    d_raw = np.abs(raw - exp_raw)
    d_cal = np.abs(cal - exp_cal)
    bad = np.where((d_raw > tolerance) | (d_cal > tolerance))[0]

    print(
        f"parity fixtures: {len(rows)} authors, "
        f"max raw err {d_raw.max():.3e}, max calibrated err {d_cal.max():.3e}, "
        f"{len(bad)} over tolerance {tolerance:g}"
    )
    for i in bad[:10]:
        print(
            f"  FAIL {rows[i]['pubkey'][:16]}… "
            f"raw {raw[i]:.9f} vs {exp_raw[i]:.9f} (Δ{d_raw[i]:.3e})  "
            f"cal {cal[i]:.9f} vs {exp_cal[i]:.9f} (Δ{d_cal[i]:.3e})"
        )

    if len(bad) == 0:
        # A sanity check on the fixtures themselves: labels should separate.
        labels = np.array([r["label"] for r in rows])
        if labels.max() > 0 and labels.min() == 0:
            print(
                f"  label separation: mean score {cal[labels == 1].mean():.4f} (bot) "
                f"vs {cal[labels == 0].mean():.4f} (real)"
            )
    return len(bad) == 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--tolerance", type=float, default=1e-6)
    ap.add_argument("--model-dir", default=None)
    args = ap.parse_args()

    model = Nspam.load(args.model_dir)
    print(f"model {model.model_version}, {model.total_features} features\n")

    ok_hash = check_hashes(model)
    if not ok_hash:
        print("\nHASH PARITY FAILED — fix hashing before looking at anything else.")
        return 1

    ok_parity = check_parity(model, args.tolerance)
    print("\n" + ("PARITY OK" if ok_parity else "PARITY FAILED"))
    return 0 if ok_parity else 1


if __name__ == "__main__":
    sys.exit(main())
