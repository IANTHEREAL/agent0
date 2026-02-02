-- SERIAL/IDENTITY rename regression test.
--
-- Repro:
--   CREATE TABLE t(a SERIAL);
--   ALTER TABLE t RENAME TO t2;
--   INSERT INTO t2 DEFAULT VALUES;
--
-- The implicit sequence object is not renamed when the table/column is renamed,
-- so SERIAL autofill must resolve the owned sequence identity rather than
-- recomputing the implicit sequence name from current table/column names.

DROP TABLE IF EXISTS serial_rename_t2;
DROP TABLE IF EXISTS serial_rename_t;

CREATE TABLE serial_rename_t(a SERIAL);

ALTER TABLE serial_rename_t RENAME TO serial_rename_t2;
INSERT INTO serial_rename_t2 DEFAULT VALUES;

SELECT count(*) AS n_after_table_rename FROM serial_rename_t2;

ALTER TABLE serial_rename_t2 RENAME COLUMN a TO b;
INSERT INTO serial_rename_t2 DEFAULT VALUES;

SELECT
  count(*) AS n_after_column_rename,
  min(b) AS min_b,
  max(b) AS max_b
FROM serial_rename_t2;

DROP TABLE serial_rename_t2;
