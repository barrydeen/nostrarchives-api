-- Backfill zap amounts from kind-9735 zap receipt events.
-- The amount is embedded in the "description" tag which contains a JSON-encoded
-- kind-9734 zap request. We extract the "amount" tag from that JSON.
INSERT INTO event_tags (event_id, tag_name, tag_value, extra_values)
SELECT
    e.id,
    'amount',
    zap_amount.amount,
    '[]'::jsonb
FROM events e
CROSS JOIN LATERAL (
    SELECT (e.tags -> idx.i) ->> 1 AS desc_json
    FROM generate_series(0, jsonb_array_length(e.tags) - 1) AS idx(i)
    WHERE (e.tags -> idx.i) ->> 0 = 'description'
    LIMIT 1
) AS desc_tag
CROSS JOIN LATERAL (
    SELECT tag_arr ->> 1 AS amount
    FROM jsonb_array_elements(desc_tag.desc_json::jsonb -> 'tags') AS tag_arr
    WHERE tag_arr ->> 0 = 'amount'
    LIMIT 1
) AS zap_amount
WHERE e.kind = 9735
  AND zap_amount.amount IS NOT NULL
  AND zap_amount.amount ~ '^[0-9]+$'
ON CONFLICT DO NOTHING;
