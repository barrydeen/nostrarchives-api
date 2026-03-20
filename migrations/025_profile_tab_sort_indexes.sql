-- no-transaction
-- Migration 025: Add per-pubkey engagement sort indexes for profile notes/replies tabs
--
-- Problem: profile_notes/replies with sort=likes/zaps/reposts require scanning all of a
-- user's notes to sort by engagement before applying LIMIT, because the existing partial
-- indexes only cover (pubkey, created_at DESC). For prolific authors this is a full fan-out.
--
-- Fix: Add composite partial indexes with the engagement columns so PostgreSQL can serve
-- the top-N rows directly without a full sort.
--
-- Uses CONCURRENTLY to avoid blocking writes during index builds on large tables.
-- Requires no-transaction mode (sqlx will not wrap this in a transaction).

-- Notes: likes sort
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_events_profile_notes_likes
    ON events (pubkey, reaction_count DESC, created_at DESC)
    WHERE kind = 1 AND NOT is_reply;

-- Notes: zaps sort
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_events_profile_notes_zaps
    ON events (pubkey, zap_amount_msats DESC, created_at DESC)
    WHERE kind = 1 AND NOT is_reply;

-- Notes: reposts sort
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_events_profile_notes_reposts
    ON events (pubkey, repost_count DESC, created_at DESC)
    WHERE kind = 1 AND NOT is_reply;

-- Replies: likes sort
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_events_profile_replies_likes
    ON events (pubkey, reaction_count DESC, created_at DESC)
    WHERE kind = 1 AND is_reply;

-- Replies: zaps sort
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_events_profile_replies_zaps
    ON events (pubkey, zap_amount_msats DESC, created_at DESC)
    WHERE kind = 1 AND is_reply;

-- Replies: reposts sort
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_events_profile_replies_reposts
    ON events (pubkey, repost_count DESC, created_at DESC)
    WHERE kind = 1 AND is_reply;
