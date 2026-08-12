#!/bin/bash
#
# Ongoing bot moderation. Scores authors with new reply activity, then bans only
# the most extreme scores automatically and leaves the rest for human review.
#
# Add to crontab (after the daily analytics job):
#   0 3 * * * /opt/apps/nostr-api/scripts/nspam/moderate.sh
#
# Deliberately conservative. The model card reports precision 0.927 at 90%
# recall — roughly 1 in 14 flagged authors would be wrong at that operating
# point. That is acceptable for a batch a human reviews and unacceptable for an
# unattended nightly job, so AUTOBAN_THRESHOLD sits far above the review
# threshold and MAX_AUTO_BANS caps the blast radius.
#
# Set AUTOBAN_THRESHOLD=2 to disable auto-banning entirely and queue everything
# for review.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
VENV="$SCRIPT_DIR/.venv/bin/python"
LOG_FILE="${NSPAM_LOG:-/var/log/nostr-api-nspam.log}"

# Tune these from the Phase 1 distribution review, not from guesswork.
# Which notes feed the model. 'replies' is in-distribution; 'roots'/'all' are
# not, so give them their own (higher) threshold via AUTOBAN_THRESHOLD.
# Banning is author-level, so a bot caught on replies loses its root notes too.
NOTE_MODE="${NOTE_MODE:-replies}"
AUTOBAN_THRESHOLD="${AUTOBAN_THRESHOLD:-0.995}"
MIN_REPLIES="${MIN_REPLIES:-5}"
MAX_FOLLOWERS="${MAX_FOLLOWERS:-100}"
MAX_AUTO_BANS="${MAX_AUTO_BANS:-200}"

mkdir -p "$(dirname "$LOG_FILE")"
log() { echo "[$(date +'%Y-%m-%d %H:%M:%S')] $*" | tee -a "$LOG_FILE"; }

if [ ! -x "$VENV" ]; then
    log "ERROR: venv missing at $VENV — run: python3 -m venv .venv && .venv/bin/pip install -r requirements.txt"
    exit 1
fi

log "nspam moderation starting (mode=$NOTE_MODE, autoban>=$AUTOBAN_THRESHOLD)"

# 1. Score authors with new replies since their last score. The parity gate runs
#    inside this and aborts the whole thing if featurization has drifted.
log "scoring authors with new $NOTE_MODE activity…"
if ! "$VENV" "$SCRIPT_DIR/score_authors.py" --notes "$NOTE_MODE" --incremental --execute >>"$LOG_FILE" 2>&1; then
    log "ERROR: scoring failed — not proceeding to any bans"
    exit 1
fi

# 2. Confirm only the extreme tail. Guardrails (allowlist, follower exemption,
#    reply-count floor, population-fraction limit) are enforced inside promote.
log "promoting authors above $AUTOBAN_THRESHOLD (cap $MAX_AUTO_BANS)…"
if ! yes "confirm $MAX_AUTO_BANS" | "$VENV" "$SCRIPT_DIR/review.py" \
        --scored-on "$NOTE_MODE" promote \
        --threshold "$AUTOBAN_THRESHOLD" \
        --max-bans "$MAX_AUTO_BANS" \
        --min-replies "$MIN_REPLIES" \
        --max-followers "$MAX_FOLLOWERS" \
        --execute >>"$LOG_FILE" 2>&1; then
    log "promote step reported no action or was refused by a guardrail"
fi

# 3. Ban and purge whatever is now confirmed. Archives before deleting.
log "purging confirmed bots…"
ARCHIVE="${NSPAM_ARCHIVE_DIR:-/var/backups/nspam}/purge-$(date +%Y%m%d-%H%M%S).jsonl"
mkdir -p "$(dirname "$ARCHIVE")"
cd "$REPO_DIR"
if ! cargo run --release --quiet --bin ban_bots -- \
        --execute --max-bans "$MAX_AUTO_BANS" --archive "$ARCHIVE" >>"$LOG_FILE" 2>&1; then
    log "ERROR: ban_bots failed"
    exit 1
fi

log "nspam moderation finished — review pending authors with: review.py hist"
