-- Migration 026: Index zap_metadata.created_at for time-range aggregation queries.
--
-- daily_stats and compute_daily_analytics query zap_metadata filtered only by
-- created_at (no sender/recipient). Without this index Postgres does a full
-- seq scan on the zap_metadata table for every 24h stats request.

CREATE INDEX IF NOT EXISTS idx_zap_metadata_created_at
    ON zap_metadata (created_at DESC);
