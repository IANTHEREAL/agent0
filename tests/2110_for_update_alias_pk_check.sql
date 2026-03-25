-- Issue #2110: FOR UPDATE/SHARE PK validation must work with table aliases.
-- The PK-requirement check in postprocess.rs must use schema_map_key()
-- (which includes the alias) to look up table schemas, matching the
-- insertion path in pipeline.rs.

-- 1) Table WITH PK — FOR UPDATE with alias should succeed.
DROP TABLE IF EXISTS for_update_alias_2110;
CREATE TABLE for_update_alias_2110 (id INT PRIMARY KEY, val TEXT);
INSERT INTO for_update_alias_2110 VALUES (1, 'a'), (2, 'b'), (3, 'c');

SELECT t.id, t.val FROM for_update_alias_2110 AS t ORDER BY t.id FOR UPDATE;

-- 2) Same table, no alias — should also succeed.
SELECT id, val FROM for_update_alias_2110 ORDER BY id FOR UPDATE;

-- 3) FOR SHARE with alias — should succeed.
SELECT t.id FROM for_update_alias_2110 AS t ORDER BY t.id FOR SHARE;

DROP TABLE for_update_alias_2110;

-- 4) Table WITHOUT PK — FOR UPDATE with alias MUST error.
--    This is the core regression guard: if the schema_map_key lookup
--    is broken, the PK check is silently skipped and this query would
--    incorrectly succeed instead of returning an error.
DROP TABLE IF EXISTS no_pk_2110;
CREATE TABLE no_pk_2110 (a INT, b TEXT);
INSERT INTO no_pk_2110 VALUES (1, 'x');

SELECT * FROM no_pk_2110 AS t FOR UPDATE;

-- 5) Same table, no alias — should also error.
SELECT * FROM no_pk_2110 FOR UPDATE;

DROP TABLE no_pk_2110;
