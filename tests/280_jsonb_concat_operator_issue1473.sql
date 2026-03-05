-- Issue #1473: JSONB `||` must merge JSON objects (PostgreSQL parity).
--
-- Regression: db9 previously typed `||` as text unconditionally, so
-- `jsonb || '{"k":...}'` devolved into string concatenation and produced
-- invalid JSON output.

DROP TABLE IF EXISTS jsonb_concat_events;
CREATE TABLE jsonb_concat_events (
  id INT PRIMARY KEY,
  payload JSONB NOT NULL
);

INSERT INTO jsonb_concat_events (id, payload)
VALUES (1, '{"type":"click","page":"/home"}');

-- Explicit jsonb RHS.
SELECT payload || '{"new":true}'::jsonb AS merged
FROM jsonb_concat_events
ORDER BY id;

-- Untyped string literal RHS must be coerced to jsonb when the other side is jsonb.
SELECT payload || '{"new":true}' AS merged_unknown_literal
FROM jsonb_concat_events
ORDER BY id;

-- Simple object merge.
SELECT '{"a":1}'::jsonb || '{"bb":2}'::jsonb AS obj_obj;

DROP TABLE jsonb_concat_events;

