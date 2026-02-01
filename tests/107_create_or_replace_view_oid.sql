-- Issue #61 regression test: CREATE OR REPLACE VIEW must preserve OID

DROP VIEW IF EXISTS oid_replace_view;
DROP TABLE IF EXISTS oid_replace_base;
DROP TABLE IF EXISTS oid_replace_oid;

CREATE TABLE oid_replace_base (id INT);
INSERT INTO oid_replace_base VALUES (1);

CREATE VIEW oid_replace_view AS SELECT id FROM oid_replace_base;

CREATE TABLE oid_replace_oid AS
SELECT oid AS oid_before
FROM pg_catalog.pg_class
WHERE relname = 'oid_replace_view'
  AND relkind = 'v'
  AND relnamespace = (SELECT oid FROM pg_catalog.pg_namespace WHERE nspname = 'public');

CREATE OR REPLACE VIEW oid_replace_view AS
SELECT id, id + 1 AS id2 FROM oid_replace_base;

SELECT
  'oid_preserved=' ||
  CASE
    WHEN (
      SELECT oid
      FROM pg_catalog.pg_class
      WHERE relname = 'oid_replace_view'
        AND relkind = 'v'
        AND relnamespace = (SELECT oid FROM pg_catalog.pg_namespace WHERE nspname = 'public')
    ) = (SELECT oid_before FROM oid_replace_oid)
    THEN 't'
    ELSE 'f'
  END;

DROP VIEW oid_replace_view;
DROP TABLE oid_replace_oid;
DROP TABLE oid_replace_base;

