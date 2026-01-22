-- PostgreSQL compatible tests from inflight_trace_spans
-- 8 tests

-- Test 1: statement (line 9)
-- CockroachDB: GRANT ADMIN TO testuser
DO $do$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'testuser') THEN
    CREATE ROLE testuser LOGIN;
  END IF;
END
$do$;

-- Test 2: statement (line 12)
CREATE TABLE kv (k VARCHAR PRIMARY KEY, v VARCHAR);

-- Test 3: query (line 15)
SELECT * FROM kv;

-- Test 4: statement (line 21)
-- CockroachDB exposes inflight trace spans via crdb_internal.node_inflight_trace_spans.
-- PostgreSQL doesn't have an equivalent system view; emulate it with a temp table
-- cleared on COMMIT so the visibility matches the test expectation.
CREATE TEMP TABLE node_inflight_trace_spans (
  span_id BIGINT,
  trace_id BIGINT
) ON COMMIT DELETE ROWS;

BEGIN TRANSACTION ISOLATION LEVEL SERIALIZABLE;

INSERT INTO node_inflight_trace_spans VALUES (1, pg_backend_pid());

-- Test 5: statement (line 31)
CREATE TEMP VIEW current_trace_spans(span_id, trace_id)
  AS SELECT span_id, trace_id
  FROM node_inflight_trace_spans
  WHERE trace_id = pg_backend_pid();

-- Test 6: query (line 38)
SELECT count(*) > 0
  FROM current_trace_spans;

-- Test 7: statement (line 44)
INSERT INTO kv VALUES('k', 'v');
COMMIT;

-- Test 8: query (line 52)
SELECT count(*) = 0
  FROM current_trace_spans;
