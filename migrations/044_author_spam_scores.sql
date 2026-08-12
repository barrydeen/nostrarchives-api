-- Per-author reply-spam scores produced by the nspam classifier
-- (barrydeen/nspam, LightGBM over hashed char/word n-grams + structural features).
--
-- Scoring is done out-of-band by scripts/nspam/score_authors.py, which reads the
-- author's most recent reply notes and writes a calibrated probability here.
--
-- `decision` is the review gate: nothing is ever banned or purged straight off
-- `score`. An operator (or the auto-ban loop, above a high threshold) promotes a
-- row to 'confirmed', and only then does the ban/purge path act on it.
--   pending   — scored, awaiting a decision
--   confirmed — reviewed and judged a bot; eligible for ban + purge
--   cleared   — reviewed and judged legitimate; a rescore never reopens this
--   exempt    — blocked from banning by a guardrail (allowlist, follower count),
--               recorded rather than silently skipped so review can see why
--   purged    — ban + purge has been carried out
CREATE TABLE IF NOT EXISTS author_spam_scores (
    pubkey           TEXT NOT NULL,
    score            REAL NOT NULL,
    raw_score        REAL NOT NULL,
    n_replies_scored SMALLINT NOT NULL,
    newest_reply_at  BIGINT NOT NULL DEFAULT 0,
    total_replies    INTEGER NOT NULL DEFAULT 0,
    -- Which kind-1 notes fed the model: 'replies', 'roots', or 'all'. The model
    -- is trained on replies, so a score produced from root notes is
    -- out-of-distribution and needs its own threshold — recorded here so review
    -- can separate them rather than pooling incomparable scores.
    scored_on        TEXT NOT NULL DEFAULT 'replies',
    follower_count   INTEGER NOT NULL DEFAULT 0,
    model_version    TEXT NOT NULL,
    scored_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    decision         TEXT NOT NULL DEFAULT 'pending',
    decided_at       TIMESTAMPTZ,
    decided_by       TEXT,
    decision_reason  TEXT,
    CONSTRAINT author_spam_scores_score_range CHECK (score >= 0 AND score <= 1),
    CONSTRAINT author_spam_scores_decision_valid
        CHECK (decision IN ('pending', 'confirmed', 'cleared', 'exempt', 'purged')),
    CONSTRAINT author_spam_scores_scored_on_valid
        CHECK (scored_on IN ('replies', 'roots', 'all')),
    -- Keyed on (pubkey, scored_on), not pubkey alone: an author can hold one
    -- score per note mode. With pubkey as the sole key, running the root pass
    -- silently overwrites the reply score — and the reply score is the
    -- in-distribution one that actually works.
    PRIMARY KEY (pubkey, scored_on)
);

-- Review tooling sorts by score; the ban path filters on decision then score.
CREATE INDEX IF NOT EXISTS idx_author_spam_scores_score
    ON author_spam_scores (score DESC);
CREATE INDEX IF NOT EXISTS idx_author_spam_scores_decision
    ON author_spam_scores (decision, score DESC);

-- Incremental rescoring picks up the least-recently-scored authors first.
CREATE INDEX IF NOT EXISTS idx_author_spam_scores_scored_at
    ON author_spam_scores (scored_at);

-- Watermark for incremental rescoring: skip authors with no new replies.
CREATE INDEX IF NOT EXISTS idx_author_spam_scores_newest_reply
    ON author_spam_scores (newest_reply_at);

-- Append-only audit of every score ever produced. Cheap, and it lets a ban be
-- explained months later once the underlying notes have been deleted.
CREATE TABLE IF NOT EXISTS author_spam_score_history (
    id               BIGSERIAL PRIMARY KEY,
    pubkey           TEXT NOT NULL,
    score            REAL NOT NULL,
    raw_score        REAL NOT NULL,
    n_replies_scored SMALLINT NOT NULL,
    model_version    TEXT NOT NULL,
    scored_at        TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_author_spam_score_history_pubkey
    ON author_spam_score_history (pubkey, scored_at DESC);

-- Never-ban list, checked by the scorer, the promote step, and the purge tool.
CREATE TABLE IF NOT EXISTS spam_allowlist (
    pubkey   TEXT PRIMARY KEY,
    reason   TEXT,
    added_by TEXT NOT NULL,
    added_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
