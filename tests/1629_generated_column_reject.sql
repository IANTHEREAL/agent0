-- Issue #1629: PostgreSQL parity for computed generated columns.

DROP TABLE IF EXISTS gen_col_test;
DROP TABLE IF EXISTS gen_col_alter_test;

-- 1) CREATE TABLE with computed generated column should be accepted.
CREATE TABLE gen_col_test (
  a INT PRIMARY KEY,
  b INT GENERATED ALWAYS AS (a * 2) STORED
);

INSERT INTO gen_col_test(a) VALUES (1), (3);
SELECT 'create_compute=' || string_agg(b::text, ',' ORDER BY a) AS probe
FROM gen_col_test;

-- 2) ALTER TABLE ADD COLUMN with computed generated column should be accepted
--    and existing rows should be backfilled.
CREATE TABLE gen_col_alter_test (
  id INT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  val INT
);
INSERT INTO gen_col_alter_test(val) VALUES (10), (20);

ALTER TABLE gen_col_alter_test
  ADD COLUMN computed INT GENERATED ALWAYS AS (val * 10) STORED;

SELECT 'alter_backfill=' || string_agg(computed::text, ',' ORDER BY id) AS probe
FROM gen_col_alter_test;

INSERT INTO gen_col_alter_test(val) VALUES (30);
SELECT 'alter_insert=' || computed::text AS probe
FROM gen_col_alter_test
WHERE val = 30;

UPDATE gen_col_alter_test SET val = 7 WHERE val = 20;
SELECT 'alter_update=' || computed::text AS probe
FROM gen_col_alter_test
WHERE val = 7;

DROP TABLE gen_col_test;
DROP TABLE gen_col_alter_test;
