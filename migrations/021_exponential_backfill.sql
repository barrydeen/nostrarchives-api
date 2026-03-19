-- Add current_window_secs to track exponential backoff between cycles.
-- Reset fully_backfilled so all relays resume backfilling with the new logic.
ALTER TABLE negentropy_sync_state
    ADD COLUMN IF NOT EXISTS current_window_secs BIGINT NOT NULL DEFAULT 86400;

-- Reset all backfill state so relays get re-evaluated with exponential windows
UPDATE negentropy_sync_state SET fully_backfilled = false, consecutive_empty_windows = 0;
