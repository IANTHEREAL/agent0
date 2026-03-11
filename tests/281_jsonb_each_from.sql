-- Issue #1489: jsonb_each and related set-returning functions in FROM clause.
-- Covers jsonb_each, jsonb_each_text, json_each, json_each_text,
-- jsonb_array_elements, json_array_elements, jsonb_object_keys,
-- json_object_keys used as FROM-clause table sources.

-- 1. Basic jsonb_each in FROM
SELECT * FROM jsonb_each('{"a": 1, "b": 2, "c": 3}'::jsonb) ORDER BY key;

-- 2. Basic jsonb_each_text in FROM
SELECT * FROM jsonb_each_text('{"a": 1, "b": "hello", "c": true}'::jsonb) ORDER BY key;

-- 3. Basic json_each in FROM
SELECT * FROM json_each('{"x": 10, "y": 20}'::json) ORDER BY key;

-- 4. Basic json_each_text in FROM
SELECT * FROM json_each_text('{"x": 10, "y": "world"}'::json) ORDER BY key;

-- 5. jsonb_array_elements in FROM
SELECT * FROM jsonb_array_elements('[1, "two", true, null]'::jsonb);

-- 6. json_array_elements in FROM
SELECT * FROM json_array_elements('[10, 20, 30]'::json);

-- 7. jsonb_object_keys in FROM
SELECT * FROM jsonb_object_keys('{"alpha": 1, "beta": 2, "gamma": 3}'::jsonb) AS k ORDER BY k;

-- 8. json_object_keys in FROM
SELECT * FROM json_object_keys('{"foo": 1, "bar": 2}'::json) AS k ORDER BY k;

-- 9. Column aliases with jsonb_each
SELECT k, v FROM jsonb_each('{"name": "Alice", "age": 30}'::jsonb) AS t(k, v) ORDER BY k;

-- 10. Empty object
SELECT * FROM jsonb_each('{}'::jsonb);

-- 11. Nested objects
SELECT key, value FROM jsonb_each('{"outer": {"inner": 42}}'::jsonb) ORDER BY key;

-- 12. NULL input returns no rows
SELECT * FROM jsonb_each(NULL::jsonb);

-- 13. jsonb_each with table JOIN (lateral)
DROP TABLE IF EXISTS jtest CASCADE;
CREATE TABLE jtest (id INT PRIMARY KEY, data JSONB);
INSERT INTO jtest VALUES (1, '{"a": 10, "b": 20}'), (2, '{"c": 30}');

SELECT j.id, kv.key, kv.value
FROM jtest j, jsonb_each(j.data) AS kv
ORDER BY j.id, kv.key;

-- 14. jsonb_array_elements_text in FROM
SELECT * FROM jsonb_array_elements_text('["hello", "world", "test"]'::jsonb);

-- 15. jsonb_array_elements with table alias (issue #1552)
SELECT * FROM jsonb_array_elements('[1, 2, 3]'::jsonb) AS t;

-- 16. json_array_elements with table alias (issue #1552)
SELECT * FROM json_array_elements('[10, 20]'::json) AS t;

-- 17. jsonb_array_elements_text with table alias (issue #1552)
SELECT * FROM jsonb_array_elements_text('["a", "b"]'::jsonb) AS t;

-- 18. jsonb_array_elements with table alias and explicit column rename (issue #1552)
SELECT * FROM jsonb_array_elements('[1, 2]'::jsonb) AS t(elem);

DROP TABLE jtest;
