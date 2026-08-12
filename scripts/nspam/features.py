"""Feature extraction for the nspam classifier.

Port of the reference pipeline's ``features.py``, cross-checked line by line
against github.com/barrydeen/nspam-strfry (``internal/model/structural.go``,
``ngram.go``, ``model.go``). Verified end to end by ``verify_parity.py``.

Feature vector layout (total width 262167, which equals ``max_feature_idx=262166``
declared in the model file — this is arithmetic, not a guess):

    [        0 : 131072 )  char_wb 3-5 grams, hashed
    [   131072 : 262144 )  word 1-2 grams, hashed
    [   262144 : 262161 )  17 structural features, mean-aggregated over the bundle
    [   262161 : 262167 )  6 group features

Two things here are easy to get wrong and are the reason the parity gate exists:

* The **whole bundle is scored as one document**. Every note's text is joined
  with a single space and vectorized once — not vectorized per note and summed.
* The **char analyzer reads ``raw_text``** (NFKC only, invisibles preserved)
  while the **word analyzer reads ``text``** (fully normalized). Swapping them
  produces plausible but wrong scores.
"""

from __future__ import annotations

import math
from typing import Any, Iterable, Sequence

import numpy as np
import regex
from scipy import sparse
from sklearn.feature_extraction.text import HashingVectorizer

from preprocess import body_key, count_invisible, preprocess, tokenize

# ── Regexes, mirroring the reference features.py ──────────────────────────────
# Note this URL pattern differs from preprocess._URL_RE: it captures the host
# only, with no optional path group.
_STRUCT_URL_RE = regex.compile(r"https?://([^\s/]+)", regex.IGNORECASE)
_MENTION_RE = regex.compile(
    r"\b(?:nostr:)?(?:npub1|note1|nprofile1|nevent1|naddr1)[0-9a-z]+", regex.IGNORECASE
)
_HASHTAG_RE = regex.compile(r"#\w+")
_DIGIT_RE = regex.compile(r"\p{N}")
_PUNCT_RE = regex.compile(r"\p{P}")
_WS_TOKEN_RE = regex.compile(r"\S+")
_EMOJI_RE = regex.compile(r"[\p{Emoji_Presentation}\p{Extended_Pictographic}]")

N_STRUCTURAL = 17
N_GROUP = 6
N_DENSE = N_STRUCTURAL + N_GROUP


def build_vectorizers(cfg: dict[str, Any]) -> tuple[HashingVectorizer, HashingVectorizer]:
    """Construct the two hashing vectorizers from config.json.

    ``lowercase=False`` because preprocessing already casefolds (and casefold is
    not the same as lower: 'ß' folds to 'ss'). ``norm=None`` because the model
    consumes raw signed counts.
    """
    char_lo, char_hi = cfg["char_ngram_range"]
    word_lo, word_hi = cfg["word_ngram_range"]
    common = dict(
        alternate_sign=cfg["hashing"]["alternate_sign"],
        norm=None,
        binary=False,
        lowercase=False,
        strip_accents=None,
        dtype=np.float64,
    )
    char_vec = HashingVectorizer(
        analyzer=cfg["char_analyzer"],
        ngram_range=(char_lo, char_hi),
        n_features=cfg["n_features_char"],
        **common,
    )
    word_vec = HashingVectorizer(
        analyzer=cfg["word_analyzer"],
        ngram_range=(word_lo, word_hi),
        n_features=cfg["n_features_word"],
        **common,
    )
    return char_vec, word_vec


def _ratio(n: int, d: int) -> float:
    return (n / d) if d > 0 else 0.0


def extract_structural(content: str, tags: Sequence[Sequence[str]]) -> list[float]:
    """The 17 per-note structural features, computed on the RAW content.

    ``dup_body_bucket`` is always 0 at inference time (it is a training-only
    signal), matching the reference implementation.
    """
    zw = count_invisible(content)
    len_chars = len(content)
    len_tokens = len(_WS_TOKEN_RE.findall(content))

    urls = _STRUCT_URL_RE.findall(content)
    domains = {u.lower() for u in urls}

    tag_p = tag_e = tag_t = tag_other = 0
    for t in tags:
        if not t:
            continue
        name = t[0]
        if name == "p":
            tag_p += 1
        elif name == "e":
            tag_e += 1
        elif name == "t":
            tag_t += 1
        else:
            tag_other += 1

    emoji_count = len(_EMOJI_RE.findall(content))
    digit_count = len(_DIGIT_RE.findall(content))
    punct_count = len(_PUNCT_RE.findall(content))

    alpha_chars = caps_chars = 0
    for ch in content:
        if ch.isalpha():
            alpha_chars += 1
            if ch.isupper():
                caps_chars += 1

    return [
        float(len_chars),
        float(len_tokens),
        float(len(urls)),
        float(len(domains)),
        float(len(_MENTION_RE.findall(content))),
        float(len(_HASHTAG_RE.findall(content))),
        float(tag_p),
        float(tag_e),
        float(tag_t),
        float(tag_other),
        float(emoji_count),
        _ratio(emoji_count, len_chars),
        float(zw),
        _ratio(caps_chars, alpha_chars),
        _ratio(digit_count, len_chars),
        _ratio(punct_count, len_chars),
        0.0,  # dup_body_bucket — always 0 at inference
    ]


def _pop_std(xs: Sequence[float]) -> float:
    """Population standard deviation (numpy's np.std default, ddof=0)."""
    if not xs:
        return 0.0
    mean = sum(xs) / len(xs)
    return math.sqrt(sum((x - mean) ** 2 for x in xs) / len(xs))


def bundle_dense(notes: Sequence[dict[str, Any]]) -> list[float]:
    """The 23 dense features: 17 structural means followed by 6 group features."""
    if not notes:
        return [0.0] * N_DENSE

    per_note = [extract_structural(n.get("content") or "", n.get("tags") or []) for n in notes]
    inv = 1.0 / len(notes)
    agg = [sum(row[j] for row in per_note) * inv for j in range(N_STRUCTURAL)]

    contents = [n.get("content") or "" for n in notes]
    n_unique_bodies = float(len({k for k in (body_key(c) for c in contents) if k}))

    time_span_hours = 0.0
    len_chars_std = 0.0
    same_first_token_ratio = 0.0
    mean_pairwise_jaccard = 0.0

    # The reference implementation computes these only for bundles of 2+.
    if len(notes) >= 2:
        stamps = [int(n.get("created_at") or 0) for n in notes]
        stamps = [s for s in stamps if s != 0]
        if len(stamps) >= 2:
            time_span_hours = (max(stamps) - min(stamps)) / 3600.0

        len_chars_std = _pop_std([float(len(c)) for c in contents])

        token_sets: list[set[str]] = []
        first_tokens: list[str] = []
        for c in contents:
            toks = tokenize(c.casefold())
            token_sets.append(set(toks))
            if toks:
                first_tokens.append(toks[0])

        if first_tokens:
            top = max(first_tokens.count(t) for t in set(first_tokens))
            same_first_token_ratio = top / len(notes)

        sims = []
        for i in range(len(token_sets)):
            for j in range(i + 1, len(token_sets)):
                a, b = token_sets[i], token_sets[j]
                union = len(a | b)
                if union == 0:
                    continue
                sims.append(len(a & b) / union)
        if sims:
            mean_pairwise_jaccard = sum(sims) / len(sims)

    return agg + [
        float(len(notes)),
        time_span_hours,
        n_unique_bodies,
        len_chars_std,
        same_first_token_ratio,
        mean_pairwise_jaccard,
    ]


def bundle_texts(notes: Sequence[dict[str, Any]]) -> tuple[str, str]:
    """Join the bundle into the two document views the analyzers consume."""
    norm_chunks, raw_chunks = [], []
    for n in notes:
        p = preprocess(n.get("content") or "")
        norm_chunks.append(p.text)
        raw_chunks.append(p.raw_text)
    return " ".join(raw_chunks), " ".join(norm_chunks)


def build_matrix(
    bundles: Iterable[Sequence[dict[str, Any]]],
    char_vec: HashingVectorizer,
    word_vec: HashingVectorizer,
    total_features: int,
) -> sparse.csr_matrix:
    """Assemble the full design matrix, one row per author bundle."""
    bundles = list(bundles)
    raw_docs, norm_docs, dense = [], [], []
    for b in bundles:
        raw_doc, norm_doc = bundle_texts(b)
        raw_docs.append(raw_doc)
        norm_docs.append(norm_doc)
        dense.append(bundle_dense(b))

    char_block = char_vec.transform(raw_docs)
    word_block = word_vec.transform(norm_docs)
    # float32 first: the reference casts the dense block to float32 before the
    # dot product, and matching that keeps us inside parity tolerance.
    dense_block = sparse.csr_matrix(np.asarray(dense, dtype=np.float32).astype(np.float64))

    X = sparse.hstack([char_block, word_block, dense_block], format="csr")
    if X.shape[1] != total_features:
        raise ValueError(f"feature width {X.shape[1]} != expected {total_features}")
    return X
