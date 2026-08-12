"""Text normalization for the nspam classifier.

Port of the reference pipeline's ``preprocess.py``, cross-checked against the
Go implementation at github.com/barrydeen/nspam-strfry
(``internal/model/preprocess.go``).

Two text views come out of :func:`preprocess`, and they are NOT interchangeable:

* ``raw_text`` — NFKC only, invisibles preserved. Feeds the **char_wb** analyzer.
* ``text``     — NFKC, invisibles stripped, URLs collapsed to scheme+host,
                 casefolded, whitespace collapsed. Feeds the **word** analyzer.

Getting these two backwards silently produces plausible-but-wrong scores, which
is exactly what the parity fixtures exist to catch.
"""

from __future__ import annotations

import unicodedata
from dataclasses import dataclass

import regex

# ZERO_WIDTH ∪ BIDI. Matches config.json's "invisible_chars" list.
INVISIBLE = frozenset(
    "​‌‍⁠﻿᠎"
    "⁡⁢⁣⁤"
    "‪‫‬‭‮"
    "⁦⁧⁨⁩"
    "‎‏"
)

# Collapses a URL to scheme + lowercased host, discarding path and query, so
# that per-link noise doesn't blow up the word n-gram space.
_URL_RE = regex.compile(r"https?://([^\s/]+)(/\S*)?", regex.IGNORECASE)
_WS_RE = regex.compile(r"\s+")

# Used only for group features (first-token and Jaccard similarity).
_TOKEN_RE = regex.compile(r"\p{L}[\p{L}\p{M}\p{N}_]*|\p{N}+|https?://\S+|[#@]\w+")


@dataclass(frozen=True)
class Prepared:
    text: str
    raw_text: str
    zero_width: int


def count_invisible(s: str) -> int:
    return sum(1 for ch in s if ch in INVISIBLE)


def strip_invisible(s: str) -> str:
    if not any(ch in INVISIBLE for ch in s):
        return s
    return "".join(ch for ch in s if ch not in INVISIBLE)


def normalize_urls(s: str) -> str:
    return _URL_RE.sub(lambda m: "http://" + m.group(1).lower(), s)


def preprocess(text: str) -> Prepared:
    nfkc = unicodedata.normalize("NFKC", text)
    zw = count_invisible(nfkc)

    stripped = strip_invisible(nfkc)
    stripped = normalize_urls(stripped)
    stripped = stripped.casefold()
    stripped = _WS_RE.sub(" ", stripped).strip()

    return Prepared(text=stripped, raw_text=nfkc, zero_width=zw)


def tokenize(text: str) -> list[str]:
    """Group-feature tokenizer. Expects already-casefolded input."""
    return _TOKEN_RE.findall(text)


def body_key(text: str) -> str:
    """Dedup key for ``n_unique_bodies``: strip invisibles, casefold, trim,
    truncate to the first 200 code points."""
    stripped = strip_invisible(text).casefold().strip()
    return stripped[:200]
