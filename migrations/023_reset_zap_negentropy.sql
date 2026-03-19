-- Reset negentropy sync state for kind 9735 (zap receipts).
-- Previous crawler runs never fetched zaps in the per-author targeted crawl path,
-- so zap history across all relays is incomplete.
-- This forces the bulk negentropy sync to re-walk all zap history from scratch.

UPDATE negentropy_sync_state
SET
    fully_backfilled    = false,
    oldest_synced_at    = 0,
    current_window_secs = 86400
WHERE kind = 9735;
