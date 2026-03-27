-- Regression test: jsonb - text[] (multi-key deletion).
-- Previously, ARRAY['a','c'] stayed as unknown[] and failed operator lookup.
-- PostgreSQL resolves unknown[] to text[] in this operator context.

-- Test 1: literal jsonb - ARRAY literal (the sqlalchemy-smoke failure case)
SELECT '{"a":1,"b":2,"c":3,"d":4}'::jsonb - ARRAY['a','c'];

-- Test 2: column LHS - ARRAY literal
DROP TABLE IF EXISTS t_jsonb_array_delete;
CREATE TABLE t_jsonb_array_delete (id int PRIMARY KEY, payload jsonb);
INSERT INTO t_jsonb_array_delete VALUES (1, '{"a":1,"b":2,"nested":3}');
SELECT payload - ARRAY['a','nested'] FROM t_jsonb_array_delete;

-- Test 3: ensure existing single-key and index deletion still work
SELECT '{"a":1,"b":2}'::jsonb - 'a';
SELECT '[10,20,30]'::jsonb - 1;

-- Cleanup
DROP TABLE t_jsonb_array_delete;
