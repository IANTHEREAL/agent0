-- PostgreSQL compatible tests from procedure_deps
-- 76 tests
--
-- Note: The upstream CockroachDB test exercises schema-changer specific edge
-- cases (and uses Cockroach-only DDL like UPSERT). This PostgreSQL adaptation
-- focuses on the comparable behavior in PostgreSQL: dependency tracking between
-- SQL procedures and table columns, and CASCADE behavior when dropping columns.

SET client_min_messages = warning;

-- sel(): depends on cascade_sel; dropping it with CASCADE should drop the procedure.
DROP TABLE IF EXISTS t CASCADE;
DROP PROCEDURE IF EXISTS sel();
CREATE TABLE t (
  a INT PRIMARY KEY,
  b INT,
  cascade_sel INT,
  drop_sel INT
);
INSERT INTO t VALUES (1, 10, 100, 999);

CREATE PROCEDURE sel() LANGUAGE SQL AS $$
  SELECT a, b, cascade_sel FROM t ORDER BY a;
$$;

CALL sel();
ALTER TABLE t DROP COLUMN drop_sel;
SELECT to_regprocedure('sel()') IS NOT NULL AS sel_exists_after_drop_sel;
ALTER TABLE t DROP COLUMN cascade_sel CASCADE;
SELECT to_regprocedure('sel()') IS NULL AS sel_dropped_after_cascade;

-- ins(): depends on columns (a,b,def).
DROP TABLE IF EXISTS t CASCADE;
DROP PROCEDURE IF EXISTS ins();
CREATE TABLE t (
  a INT PRIMARY KEY,
  b INT,
  def INT DEFAULT 10,
  drop_ins INT
);

CREATE PROCEDURE ins() LANGUAGE SQL AS $$
  INSERT INTO t (a, b, def) VALUES (1, 10, DEFAULT);
$$;

CALL ins();
SELECT a, b, def FROM t ORDER BY a;
ALTER TABLE t DROP COLUMN drop_ins;
SELECT to_regprocedure('ins()') IS NOT NULL AS ins_exists_after_drop_ins;
ALTER TABLE t DROP COLUMN b CASCADE;
SELECT to_regprocedure('ins()') IS NULL AS ins_dropped_after_drop_b;

-- ins2(): depends on cascade_ins2; dropping it with CASCADE should drop the procedure.
DROP TABLE IF EXISTS t CASCADE;
DROP PROCEDURE IF EXISTS ins2();
CREATE TABLE t (
  a INT PRIMARY KEY,
  b INT,
  def INT DEFAULT 10,
  cascade_ins2 INT,
  drop_ins2 INT
);

CREATE PROCEDURE ins2() LANGUAGE SQL AS $$
  INSERT INTO t (a, b, cascade_ins2, def) VALUES (2, 20, 200, DEFAULT);
$$;

CALL ins2();
SELECT a, b, def, cascade_ins2 FROM t ORDER BY a;
ALTER TABLE t DROP COLUMN drop_ins2;
SELECT to_regprocedure('ins2()') IS NOT NULL AS ins2_exists_after_drop_ins2;
ALTER TABLE t DROP COLUMN cascade_ins2 CASCADE;
SELECT to_regprocedure('ins2()') IS NULL AS ins2_dropped_after_cascade;

-- ups(): Cockroach UPSERT -> PostgreSQL INSERT .. ON CONFLICT DO UPDATE.
DROP TABLE IF EXISTS t CASCADE;
DROP PROCEDURE IF EXISTS ups();
CREATE TABLE t (
  a INT PRIMARY KEY,
  b INT,
  def INT DEFAULT 10,
  drop_ups INT
);

CREATE PROCEDURE ups() LANGUAGE SQL AS $$
  INSERT INTO t (a, b, def) VALUES (3, 30, DEFAULT)
  ON CONFLICT (a) DO UPDATE SET b = EXCLUDED.b, def = EXCLUDED.def;
$$;

CALL ups();
CALL ups();
SELECT a, b, def FROM t ORDER BY a;
ALTER TABLE t DROP COLUMN drop_ups;
SELECT to_regprocedure('ups()') IS NOT NULL AS ups_exists_after_drop_ups;
ALTER TABLE t DROP COLUMN b CASCADE;
SELECT to_regprocedure('ups()') IS NULL AS ups_dropped_after_drop_b;

-- ups2(): depends on cascade_ups2; dropping it with CASCADE should drop the procedure.
DROP TABLE IF EXISTS t CASCADE;
DROP PROCEDURE IF EXISTS ups2();
CREATE TABLE t (
  a INT PRIMARY KEY,
  b INT,
  def INT DEFAULT 10,
  cascade_ups2 INT,
  drop_ups2 INT
);

CREATE PROCEDURE ups2() LANGUAGE SQL AS $$
  INSERT INTO t (a, b, cascade_ups2, def) VALUES (4, 40, 400, DEFAULT)
  ON CONFLICT (a) DO UPDATE
    SET b = EXCLUDED.b, cascade_ups2 = EXCLUDED.cascade_ups2, def = EXCLUDED.def;
$$;

CALL ups2();
CALL ups2();
SELECT a, b, def, cascade_ups2 FROM t ORDER BY a;
ALTER TABLE t DROP COLUMN drop_ups2;
SELECT to_regprocedure('ups2()') IS NOT NULL AS ups2_exists_after_drop_ups2;
ALTER TABLE t DROP COLUMN cascade_ups2 CASCADE;
SELECT to_regprocedure('ups2()') IS NULL AS ups2_dropped_after_cascade;

-- up(): depends on cascade_up; dropping it with CASCADE should drop the procedure.
DROP TABLE IF EXISTS t CASCADE;
DROP PROCEDURE IF EXISTS up();
CREATE TABLE t (
  a INT PRIMARY KEY,
  b INT,
  def INT DEFAULT 10,
  cascade_up INT,
  drop_up INT
);
INSERT INTO t (a, b, cascade_up) VALUES (1, 10, NULL);

CREATE PROCEDURE up() LANGUAGE SQL AS $$
  UPDATE t SET b = 11 WHERE a = 1 AND cascade_up IS NULL;
$$;

CALL up();
SELECT a, b, def FROM t ORDER BY a;
ALTER TABLE t DROP COLUMN drop_up;
SELECT to_regprocedure('up()') IS NOT NULL AS up_exists_after_drop_up;
ALTER TABLE t DROP COLUMN cascade_up CASCADE;
SELECT to_regprocedure('up()') IS NULL AS up_dropped_after_cascade;

-- del(): depends on cascade_del; dropping it with CASCADE should drop the procedure.
DROP TABLE IF EXISTS t CASCADE;
DROP PROCEDURE IF EXISTS del();
CREATE TABLE t (
  a INT PRIMARY KEY,
  b INT,
  def INT DEFAULT 10,
  cascade_del INT,
  drop_del INT
);
INSERT INTO t (a, b, cascade_del) VALUES (1, 10, 1), (2, 20, 0);

CREATE PROCEDURE del() LANGUAGE SQL AS $$
  DELETE FROM t WHERE a = 1 AND cascade_del = 1;
$$;

CALL del();
SELECT a, b, def FROM t ORDER BY a;
ALTER TABLE t DROP COLUMN drop_del;
SELECT to_regprocedure('del()') IS NOT NULL AS del_exists_after_drop_del;
ALTER TABLE t DROP COLUMN cascade_del CASCADE;
SELECT to_regprocedure('del()') IS NULL AS del_dropped_after_cascade;

RESET client_min_messages;

