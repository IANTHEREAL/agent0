-- Issue #407 regression: DML MUST NOT persist schema-violating Value variants.

SET client_min_messages = warning;

-- INSERT incompatible type should error and persist nothing.
DROP TABLE IF EXISTS t_int_407;
CREATE TABLE t_int_407(id INT PRIMARY KEY, i INT);

INSERT INTO t_int_407(id, i) VALUES (1, '{"a":1}'::jsonb);
SELECT 'after_bad_insert_count' AS tag, COUNT(*) AS cnt FROM t_int_407;

-- UPDATE incompatible type should error and persist nothing.
INSERT INTO t_int_407(id, i) VALUES (1, 1);
UPDATE t_int_407 SET i = ('{"a":1}'::jsonb) WHERE id = 1;
SELECT 'after_bad_update_value' AS tag, i FROM t_int_407 WHERE id = 1;

DROP TABLE t_int_407;

-- UPSERT: ON CONFLICT DO UPDATE must enforce coercion and be atomic.
DROP TABLE IF EXISTS t_upsert_407;
CREATE TABLE t_upsert_407(id INT PRIMARY KEY, v INT);

INSERT INTO t_upsert_407(id, v) VALUES (1, 1);
INSERT INTO t_upsert_407(id, v) VALUES (1, 2)
ON CONFLICT (id) DO UPDATE SET v = ('{"a":1}'::jsonb);

SELECT 'after_bad_upsert_value' AS tag, v FROM t_upsert_407 WHERE id = 1;
SELECT 'after_bad_upsert_count' AS tag, COUNT(*) AS cnt FROM t_upsert_407;

DROP TABLE t_upsert_407;
