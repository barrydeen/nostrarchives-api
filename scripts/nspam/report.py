"""Generate a browsable HTML review page for flagged authors.

The terminal output is fine for spot checks, but deciding who to ban means
reading a lot of notes and recognising accounts. This writes a self-contained
page you open in a browser: name, npub, score, the exact notes the model saw,
and a link through to njump so you can check the account yourself.

Written to a local file — it contains real people's pubkeys and note content,
so it is deliberately not published anywhere.

Called by `review.py report`.
"""

from __future__ import annotations

import html
from typing import Any

import db
from nip19 import encode_npub

CSS = """
:root{--bg:#fbfbfa;--fg:#1a1a18;--mut:#6b6b66;--line:#e3e3df;--card:#fff;
--warn:#b4341f;--ok:#2f6b3f;--chip:#f0efec}
@media(prefers-color-scheme:dark){:root{--bg:#16161a;--fg:#e8e8e6;--mut:#9a9a95;
--line:#2c2c33;--card:#1d1d22;--warn:#ff8a70;--ok:#7bc48d;--chip:#26262d}}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--fg);
font:15px/1.55 ui-sans-serif,-apple-system,Segoe UI,Roboto,sans-serif}
.wrap{max-width:1040px;margin:0 auto;padding:32px 20px 80px}
h1{font-size:24px;margin:0 0 4px;letter-spacing:-.01em}
.sub{color:var(--mut);margin:0 0 28px;font-size:14px}
.bar{display:flex;gap:10px;flex-wrap:wrap;margin:0 0 28px}
.stat{background:var(--card);border:1px solid var(--line);border-radius:10px;
padding:10px 14px;min-width:110px}
.stat b{display:block;font-size:20px;letter-spacing:-.02em}
.stat span{color:var(--mut);font-size:12px;text-transform:uppercase;letter-spacing:.05em}
.card{background:var(--card);border:1px solid var(--line);border-radius:12px;
padding:16px 18px;margin:0 0 14px}
.hd{display:flex;justify-content:space-between;gap:14px;align-items:flex-start;flex-wrap:wrap}
.nm{font-weight:600;font-size:16px}
.nm .anon{color:var(--mut);font-weight:400;font-style:italic}
.meta{color:var(--mut);font-size:13px;margin-top:2px;word-break:break-all}
.meta a{color:inherit}
.score{font-variant-numeric:tabular-nums;font-weight:600;white-space:nowrap}
.hi{color:var(--warn)}
.chips{display:flex;gap:6px;flex-wrap:wrap;margin:10px 0 0}
.chip{background:var(--chip);border-radius:999px;padding:2px 9px;font-size:12px;color:var(--mut)}
.chip.flag{color:var(--warn)}
.notes{margin:12px 0 0;padding:0;list-style:none}
.notes li{border-top:1px solid var(--line);padding:8px 0;font-size:14px;
white-space:pre-wrap;overflow-wrap:anywhere}
.notes li:first-child{border-top:0}
footer{color:var(--mut);font-size:13px;margin-top:36px;border-top:1px solid var(--line);padding-top:16px}
"""


def _chip(text: str, flag: bool = False) -> str:
    return f'<span class="chip{" flag" if flag else ""}">{html.escape(text)}</span>'


def build(conn, rows: list[dict[str, Any]], title: str, note: str) -> str:
    pubkeys = [r["pubkey"] for r in rows]
    profiles = db.fetch_profiles(conn, pubkeys)

    n_vip = sum(1 for r in rows if r["follower_count"] >= 100)
    n_thin = sum(1 for r in rows if r["n_scored"] < 10)
    total_notes = sum(r["total"] for r in rows)

    parts = [
        # Without an explicit charset a browser opening this locally falls back
        # to Latin-1 and every emoji and em-dash renders as mojibake.
        '<meta charset="utf-8">',
        '<meta name="viewport" content="width=device-width,initial-scale=1">',
        "<title>Flagged authors</title>",
        f"<style>{CSS}</style>",
        '<div class="wrap">',
        f"<h1>{html.escape(title)}</h1>",
        f'<p class="sub">{html.escape(note)}</p>',
        '<div class="bar">',
        f'<div class="stat"><b>{len(rows)}</b><span>flagged</span></div>',
        f'<div class="stat"><b>{total_notes:,}</b><span>their notes</span></div>',
        f'<div class="stat"><b>{n_vip}</b><span>100+ followers</span></div>',
        f'<div class="stat"><b>{n_thin}</b><span>thin bundle</span></div>',
        "</div>",
    ]

    for r in rows:
        pk = r["pubkey"]
        prof = profiles.get(pk, {})
        npub = encode_npub(pk)
        name = prof.get("name") or ""
        nip05 = prof.get("nip05") or ""
        name_html = (
            html.escape(name) if name else '<span class="anon">(no display name)</span>'
        )
        hi = " hi" if r["score"] >= 0.99 else ""

        chips = [_chip(f'{r["total"]:,} {r["mode"]}')]
        chips.append(_chip(f'{r["follower_count"]:,} followers',
                           flag=r["follower_count"] >= 100))
        if r["n_scored"] < 10:
            chips.append(_chip(f'thin bundle: {r["n_scored"]} notes', flag=True))
        if "mostr.pub" in nip05.lower() or "momostr" in nip05.lower():
            chips.append(_chip("bridged account", flag=True))

        notes_html = "".join(
            f"<li>{html.escape(' '.join((c or '').split())[:400])}</li>"
            for c in r["notes"]
        ) or "<li><em>no notes retrieved</em></li>"

        parts.append(
            f'<div class="card"><div class="hd"><div>'
            f'<div class="nm">{name_html}</div>'
            f'<div class="meta">{html.escape(nip05) + " · " if nip05 else ""}'
            f'<a href="https://njump.me/{npub}" target="_blank" rel="noopener">{npub}</a></div>'
            f'</div><div class="score{hi}">{r["score"]:.4f}</div></div>'
            f'<div class="chips">{"".join(chips)}</div>'
            f'<ul class="notes">{notes_html}</ul></div>'
        )

    parts.append(
        "<footer>Scores are a shortlist, not a verdict — open a few in njump before "
        "confirming. Generated from your local database; contains real pubkeys and "
        "note content, so keep it local.</footer></div>"
    )
    return "\n".join(parts)
