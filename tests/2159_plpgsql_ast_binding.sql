-- =========================
-- Phase 1 AST-aware variable binding regression test (#2159)
--
-- Pins the behavior of the AST-level PL/pgSQL variable binder.
-- Covers:
--   1A: Statement-path binding (INSERT VALUES, ON CONFLICT, UPDATE SET WHERE,
--       SELECT INTO, PERFORM)
--   1B: Expression-path binding (text, int, bool, timestamptz, jsonb, null)
--
-- Acceptance target: p_-prefixed parameters (no column/variable name collision).
-- =========================

-- ============================================================
-- 1B: Expression-path binding — typed literals
-- ============================================================

-- 1B.1  text variable
CREATE OR REPLACE FUNCTION test_bind_text(p_val text)
RETURNS text LANGUAGE plpgsql AS $$
BEGIN
  RETURN p_val;
END;
$$;

SELECT test_bind_text('hello');

-- 1B.2  int variable
CREATE OR REPLACE FUNCTION test_bind_int(p_val int)
RETURNS int LANGUAGE plpgsql AS $$
BEGIN
  RETURN p_val + 1;
END;
$$;

SELECT test_bind_int(41);

-- 1B.3  bool variable
CREATE OR REPLACE FUNCTION test_bind_bool(p_val boolean)
RETURNS boolean LANGUAGE plpgsql AS $$
BEGIN
  RETURN NOT p_val;
END;
$$;

SELECT test_bind_bool(false);

-- 1B.4  null handling
CREATE OR REPLACE FUNCTION test_bind_null()
RETURNS text LANGUAGE plpgsql AS $$
DECLARE
  x text := NULL;
BEGIN
  IF x IS NULL THEN
    RETURN 'was_null';
  END IF;
  RETURN 'not_null';
END;
$$;

SELECT test_bind_null();

-- 1B.5  jsonb variable (B2 fix: must use CAST, not raw substitution)
CREATE OR REPLACE FUNCTION test_bind_jsonb(p_data jsonb)
RETURNS text LANGUAGE plpgsql AS $$
DECLARE
  result text;
BEGIN
  result := p_data->>'name';
  RETURN result;
END;
$$;

SELECT test_bind_jsonb('{"name":"alice","age":30}'::jsonb);

-- 1B.6  jsonb in expression context (jsonb_pretty)
CREATE OR REPLACE FUNCTION test_bind_jsonb_pretty(p_data jsonb)
RETURNS text LANGUAGE plpgsql AS $$
DECLARE
  result text;
BEGIN
  result := jsonb_pretty(p_data);
  RETURN result;
END;
$$;

SELECT CASE WHEN length(test_bind_jsonb_pretty('{"k":"v"}'::jsonb)) > 0 THEN 'jsonb_pretty_ok' ELSE 'jsonb_pretty_fail' END AS result;

-- 1B.7  negative int (must not produce -- comment)
CREATE OR REPLACE FUNCTION test_bind_negative_int(p_val int)
RETURNS int LANGUAGE plpgsql AS $$
BEGIN
  RETURN p_val * 2;
END;
$$;

SELECT test_bind_negative_int(-5);

-- ============================================================
-- 1A: Statement-path binding
-- ============================================================

CREATE TABLE IF NOT EXISTS test_ast_bind_items (
  id text PRIMARY KEY,
  label text NOT NULL,
  count int NOT NULL DEFAULT 0,
  meta jsonb NOT NULL DEFAULT '{}'::jsonb
);

-- 1A.1  INSERT ... VALUES with text, int, jsonb variables
CREATE OR REPLACE FUNCTION test_insert_values(p_id text, p_label text, p_count int, p_meta jsonb)
RETURNS void LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO test_ast_bind_items(id, label, count, meta)
  VALUES (p_id, p_label, p_count, p_meta);
END;
$$;

SELECT test_insert_values('i1', 'first', 10, '{"src":"test"}'::jsonb);
SELECT id, label, count, meta->>'src' AS src FROM test_ast_bind_items WHERE id = 'i1';

-- 1A.2  INSERT ... ON CONFLICT DO UPDATE
CREATE OR REPLACE FUNCTION test_upsert(p_id text, p_label text, p_count int)
RETURNS void LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO test_ast_bind_items(id, label, count)
  VALUES (p_id, p_label, p_count)
  ON CONFLICT (id) DO UPDATE SET label = EXCLUDED.label, count = EXCLUDED.count;
END;
$$;

SELECT test_upsert('i1', 'updated', 20);
SELECT id, label, count FROM test_ast_bind_items WHERE id = 'i1';

-- 1A.3  UPDATE ... SET ... WHERE with variable in WHERE clause
CREATE OR REPLACE FUNCTION test_update_where(p_id text, p_new_label text)
RETURNS void LANGUAGE plpgsql AS $$
BEGIN
  UPDATE test_ast_bind_items SET label = p_new_label WHERE id = p_id;
END;
$$;

SELECT test_update_where('i1', 'final');
SELECT id, label FROM test_ast_bind_items WHERE id = 'i1';

-- 1A.4  SELECT INTO
CREATE OR REPLACE FUNCTION test_select_into(p_id text)
RETURNS text LANGUAGE plpgsql AS $$
DECLARE
  result_label text;
BEGIN
  SELECT label INTO result_label FROM test_ast_bind_items WHERE id = p_id;
  RETURN result_label;
END;
$$;

SELECT test_select_into('i1');

-- 1A.5  PERFORM (function call with variables)
CREATE OR REPLACE FUNCTION test_perform_target(p_x int, p_y int)
RETURNS int LANGUAGE plpgsql AS $$
BEGIN
  RETURN p_x + p_y;
END;
$$;

CREATE OR REPLACE FUNCTION test_perform_caller(p_a int, p_b int)
RETURNS void LANGUAGE plpgsql AS $$
BEGIN
  PERFORM test_perform_target(p_a, p_b);
END;
$$;

SELECT test_perform_caller(3, 4);

-- 1A.6  DECLARE expression binding (variable initialized from expression with other variable)
CREATE OR REPLACE FUNCTION test_declare_expr(p_base text)
RETURNS text LANGUAGE plpgsql AS $$
DECLARE
  full_path text := '/root/' || p_base || '/data';
BEGIN
  RETURN full_path;
END;
$$;

SELECT test_declare_expr('project-x');

-- 1A.7  DECLARE variable chaining (local var depends on prior local var)
CREATE OR REPLACE FUNCTION test_declare_chain(p_id text)
RETURNS text LANGUAGE plpgsql AS $$
DECLARE
  root text := '/swarm/' || p_id;
  claim_dir text := root || '/claims';
  claim_path text := claim_dir || '/' || p_id || '.json';
BEGIN
  RETURN claim_path;
END;
$$;

SELECT test_declare_chain('proj-1');

-- ============================================================
-- Verify table state (pins 1A correctness)
-- ============================================================
SELECT id, label, count FROM test_ast_bind_items ORDER BY id;

-- ============================================================
-- Cleanup
-- ============================================================
DROP FUNCTION IF EXISTS test_bind_text;
DROP FUNCTION IF EXISTS test_bind_int;
DROP FUNCTION IF EXISTS test_bind_bool;
DROP FUNCTION IF EXISTS test_bind_null;
DROP FUNCTION IF EXISTS test_bind_jsonb;
DROP FUNCTION IF EXISTS test_bind_jsonb_pretty;
DROP FUNCTION IF EXISTS test_bind_negative_int;
DROP FUNCTION IF EXISTS test_insert_values;
DROP FUNCTION IF EXISTS test_upsert;
DROP FUNCTION IF EXISTS test_update_where;
DROP FUNCTION IF EXISTS test_select_into;
DROP FUNCTION IF EXISTS test_perform_target;
DROP FUNCTION IF EXISTS test_perform_caller;
DROP FUNCTION IF EXISTS test_declare_expr;
DROP FUNCTION IF EXISTS test_declare_chain;
DROP TABLE IF EXISTS test_ast_bind_items;
