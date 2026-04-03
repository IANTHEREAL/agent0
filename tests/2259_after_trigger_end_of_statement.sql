-- Regression test for #2259: AFTER row-level triggers must fire after all
-- row modifications in the statement complete (PostgreSQL semantics).
-- Each trigger invocation should see the final post-statement table state.
--
-- Strategy: trigger logs SUM(val). With deferred (correct) execution, ALL
-- trigger invocations see the same final SUM. With inline (bug) execution,
-- each invocation sees a different partial SUM.  We detect the difference
-- by counting DISTINCT observed sums — deferred always produces 1.

DROP TABLE IF EXISTS trigger_log_2259;
DROP TABLE IF EXISTS t_2259;

CREATE TABLE t_2259 (id INT PRIMARY KEY, val INT);
CREATE TABLE trigger_log_2259 (
  id SERIAL PRIMARY KEY,
  op TEXT,
  observed_sum INT
);

CREATE FUNCTION log_sum_2259() RETURNS TRIGGER AS $$
DECLARE
  s INT;
BEGIN
  SELECT COALESCE(SUM(val), 0) INTO s FROM t_2259;
  INSERT INTO trigger_log_2259 (op, observed_sum) VALUES (TG_OP, s);
  RETURN NEW;
END;
$$ LANGUAGE plpgsql;

-- Test 1: INSERT
CREATE TRIGGER after_insert_2259
  AFTER INSERT ON t_2259
  FOR EACH ROW EXECUTE FUNCTION log_sum_2259();

INSERT INTO t_2259 VALUES (1, 10), (2, 20), (3, 30);

-- Deferred: 1 distinct sum (60). Inline bug: 3 distinct sums (10, 30, 60).
SELECT COUNT(*) AS fire_count, COUNT(DISTINCT observed_sum) AS distinct_sums, MIN(observed_sum) AS observed FROM trigger_log_2259;

TRUNCATE trigger_log_2259;
DROP TRIGGER after_insert_2259 ON t_2259;

-- Test 2: UPDATE (batch path, no BEFORE triggers)
CREATE TRIGGER after_update_2259
  AFTER UPDATE ON t_2259
  FOR EACH ROW EXECUTE FUNCTION log_sum_2259();

UPDATE t_2259 SET val = val + 1;

SELECT COUNT(*) AS fire_count, COUNT(DISTINCT observed_sum) AS distinct_sums, MIN(observed_sum) AS observed FROM trigger_log_2259;

TRUNCATE trigger_log_2259;
DROP TRIGGER after_update_2259 ON t_2259;

-- Test 2b: UPDATE (per-row path, BEFORE trigger forces fallback)
CREATE FUNCTION noop_before_2259() RETURNS TRIGGER AS $$
BEGIN
  RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER before_update_2259
  BEFORE UPDATE ON t_2259
  FOR EACH ROW EXECUTE FUNCTION noop_before_2259();

CREATE TRIGGER after_update_perrow_2259
  AFTER UPDATE ON t_2259
  FOR EACH ROW EXECUTE FUNCTION log_sum_2259();

UPDATE t_2259 SET val = val + 1;

SELECT COUNT(*) AS fire_count, COUNT(DISTINCT observed_sum) AS distinct_sums, MIN(observed_sum) AS observed FROM trigger_log_2259;

TRUNCATE trigger_log_2259;
DROP TRIGGER before_update_2259 ON t_2259;
DROP TRIGGER after_update_perrow_2259 ON t_2259;

-- Test 3: DELETE
CREATE TRIGGER after_delete_2259
  AFTER DELETE ON t_2259
  FOR EACH ROW EXECUTE FUNCTION log_sum_2259();

DELETE FROM t_2259;

SELECT COUNT(*) AS fire_count, COUNT(DISTINCT observed_sum) AS distinct_sums, MIN(observed_sum) AS observed FROM trigger_log_2259;

DROP TRIGGER after_delete_2259 ON t_2259;
TRUNCATE trigger_log_2259;

-- Test 4: INSERT ON CONFLICT DO UPDATE
INSERT INTO t_2259 VALUES (1, 100), (2, 200), (3, 300);

CREATE TRIGGER after_upsert_update_2259
  AFTER UPDATE ON t_2259
  FOR EACH ROW EXECUTE FUNCTION log_sum_2259();

INSERT INTO t_2259 VALUES (1, 101), (2, 201), (3, 301)
  ON CONFLICT (id) DO UPDATE SET val = EXCLUDED.val;

SELECT COUNT(*) AS fire_count, COUNT(DISTINCT observed_sum) AS distinct_sums, MIN(observed_sum) AS observed FROM trigger_log_2259;

-- Final cleanup
DROP TABLE trigger_log_2259;
DROP TABLE t_2259;
DROP FUNCTION log_sum_2259;
DROP FUNCTION noop_before_2259;
