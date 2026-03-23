-- Drop the unused event_tags table.
-- Tags are stored in the events.tags JSONB column and queried via GIN indexes.
-- event_tags was never populated and exists only as dead schema.

DROP TABLE IF EXISTS event_tags;
