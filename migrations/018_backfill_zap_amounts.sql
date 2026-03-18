-- Migration 018: Placeholder for zap amount backfill
-- The actual backfill logic is implemented in Rust and called at startup
-- This migration serves as a version marker

-- No schema changes needed
-- The backfill_zero_amount_zaps function in repository.rs will:
-- 1. Select zap_metadata rows with amount_msats = 0
-- 2. Parse bolt11 tags from corresponding events
-- 3. Update amount_msats with parsed values