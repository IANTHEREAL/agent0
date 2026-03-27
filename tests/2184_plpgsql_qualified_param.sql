-- =========================
-- PL/pgSQL: function_name.param_name qualified parameter binding (#2184)
--
-- PostgreSQL allows disambiguating function parameters from column names
-- using `function_name.param_name` syntax. This test validates that the
-- AST binder correctly resolves these qualified references.
-- =========================

-- Setup: a table with columns that will collide with parameter names
CREATE TABLE IF NOT EXISTS qp_items (
  project_id text NOT NULL,
  task_id text NOT NULL,
  title text NOT NULL DEFAULT '',
  status text NOT NULL DEFAULT 'open',
  priority int NOT NULL DEFAULT 0,
  PRIMARY KEY (project_id, task_id)
);

-- Clean slate
DELETE FROM qp_items;

-- ============================================================
-- Test 1: INSERT + qualified param in WHERE of subquery
-- ============================================================
CREATE OR REPLACE FUNCTION qp_insert_item(
  project_id text,
  task_id text,
  title text
) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO qp_items(project_id, task_id, title)
  VALUES (qp_insert_item.project_id, qp_insert_item.task_id, qp_insert_item.title);
END;
$$;

SELECT qp_insert_item('proj-1', 'T1', 'first task');
SELECT qp_insert_item('proj-1', 'T2', 'second task');
SELECT * FROM qp_items ORDER BY task_id;

-- ============================================================
-- Test 2: UPDATE with qualified param in WHERE clause
-- ============================================================
CREATE OR REPLACE FUNCTION qp_update_status(
  project_id text,
  task_id text,
  status text
) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
  UPDATE qp_items
    SET status = qp_update_status.status
  WHERE qp_items.project_id = qp_update_status.project_id
    AND qp_items.task_id = qp_update_status.task_id;
END;
$$;

SELECT qp_update_status('proj-1', 'T1', 'done');
SELECT task_id, status FROM qp_items ORDER BY task_id;

-- ============================================================
-- Test 3: SELECT with qualified param in WHERE (return query result)
-- ============================================================
CREATE OR REPLACE FUNCTION qp_count_by_project(
  project_id text
) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
  cnt bigint;
BEGIN
  SELECT count(*) INTO cnt
  FROM qp_items t
  WHERE t.project_id = qp_count_by_project.project_id;
  RETURN cnt;
END;
$$;

SELECT qp_count_by_project('proj-1');

-- ============================================================
-- Test 4: DELETE with qualified param
-- ============================================================
CREATE OR REPLACE FUNCTION qp_delete_task(
  project_id text,
  task_id text
) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
  DELETE FROM qp_items
  WHERE qp_items.project_id = qp_delete_task.project_id
    AND qp_items.task_id = qp_delete_task.task_id;
END;
$$;

SELECT qp_delete_task('proj-1', 'T2');
SELECT * FROM qp_items ORDER BY task_id;

-- ============================================================
-- Test 5: Mixed qualified + bare params in same function
-- ============================================================
CREATE OR REPLACE FUNCTION qp_mixed_refs(
  project_id text,
  task_id text,
  title text,
  priority int
) RETURNS text LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO qp_items(project_id, task_id, title, priority)
  VALUES (qp_mixed_refs.project_id, qp_mixed_refs.task_id, title, priority);
  RETURN 'ok';
END;
$$;

SELECT qp_mixed_refs('proj-2', 'T10', 'mixed test', 5);
SELECT * FROM qp_items WHERE project_id = 'proj-2' ORDER BY task_id;

-- ============================================================
-- Test 6: INSERT ON CONFLICT with qualified params
-- ============================================================
CREATE OR REPLACE FUNCTION qp_upsert_item(
  project_id text,
  task_id text,
  title text,
  priority int
) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO qp_items(project_id, task_id, title, priority)
  VALUES (qp_upsert_item.project_id, qp_upsert_item.task_id, qp_upsert_item.title, qp_upsert_item.priority)
  ON CONFLICT (project_id, task_id) DO UPDATE
    SET title = EXCLUDED.title,
        priority = EXCLUDED.priority;
END;
$$;

SELECT qp_upsert_item('proj-1', 'T1', 'updated title', 9);
SELECT task_id, title, priority FROM qp_items WHERE project_id = 'proj-1' ORDER BY task_id;

-- ============================================================
-- Test 7: EXISTS subquery with qualified params (swarm_try_claim pattern)
-- ============================================================
CREATE OR REPLACE FUNCTION qp_check_exists(
  project_id text,
  task_id text
) RETURNS boolean LANGUAGE plpgsql AS $$
DECLARE
  found boolean := false;
BEGIN
  IF EXISTS (
    SELECT 1 FROM qp_items t
    WHERE t.project_id = qp_check_exists.project_id
      AND t.task_id = qp_check_exists.task_id
  ) THEN
    found := true;
  END IF;
  RETURN found;
END;
$$;

SELECT qp_check_exists('proj-1', 'T1');
SELECT qp_check_exists('proj-1', 'T999');

-- Cleanup
DROP FUNCTION IF EXISTS qp_insert_item(text, text, text);
DROP FUNCTION IF EXISTS qp_update_status(text, text, text);
DROP FUNCTION IF EXISTS qp_count_by_project(text);
DROP FUNCTION IF EXISTS qp_delete_task(text, text);
DROP FUNCTION IF EXISTS qp_mixed_refs(text, text, text, int);
DROP FUNCTION IF EXISTS qp_upsert_item(text, text, text, int);
DROP FUNCTION IF EXISTS qp_check_exists(text, text);
DROP TABLE IF EXISTS qp_items;
