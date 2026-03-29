-- Materialized view: top 100 users per client, ordered by note count.
-- Powers the /v1/clients/:client_name/users drill-down endpoint.
-- Refreshed alongside other analytics MVs every 30 minutes.

CREATE MATERIALIZED VIEW IF NOT EXISTS mv_client_top_users AS
WITH client_notes AS (
    SELECT
        LOWER(tag_elem->>1)          AS client_name,
        e.pubkey,
        COUNT(DISTINCT e.id)::bigint AS note_count,
        MIN(e.created_at)::bigint    AS first_seen,
        MAX(e.created_at)::bigint    AS last_seen
    FROM events e
    JOIN profile_search ps ON ps.pubkey = e.pubkey AND ps.follower_count >= 1,
         jsonb_array_elements(e.tags) AS tag_elem
    WHERE tag_elem->>0 = 'client'
      AND e.kind = 1
      AND LENGTH(tag_elem->>1) BETWEEN 1 AND 100
      AND LOWER(tag_elem->>1) NOT IN ('mostr')
    GROUP BY LOWER(tag_elem->>1), e.pubkey
),
ranked AS (
    SELECT *,
           ROW_NUMBER() OVER (PARTITION BY client_name ORDER BY note_count DESC) AS rn
    FROM client_notes
)
SELECT client_name, pubkey, note_count, first_seen, last_seen
FROM ranked
WHERE rn <= 100;

CREATE UNIQUE INDEX IF NOT EXISTS idx_mv_client_top_users_pk
    ON mv_client_top_users (client_name, pubkey);

CREATE INDEX IF NOT EXISTS idx_mv_client_top_users_client
    ON mv_client_top_users (client_name, note_count DESC);
