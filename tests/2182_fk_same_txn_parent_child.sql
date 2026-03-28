-- Regression test: INSERT parent + child with FK in the same transaction.
-- PR #2182 introduced pessimistic locks for FK validation; batch_get_for_update
-- bypasses the client buffer, so same-txn parent rows must be found via a
-- fallback buffer check (#2192).

CREATE TABLE fk_ryow_parent (id INT PRIMARY KEY);
CREATE TABLE fk_ryow_child (id INT PRIMARY KEY, pid INT REFERENCES fk_ryow_parent(id));

-- Same-txn INSERT parent then child — must succeed, not FK violation.
BEGIN;
INSERT INTO fk_ryow_parent VALUES (1);
INSERT INTO fk_ryow_child VALUES (1, 1);
COMMIT;

-- Verify both rows persisted.
SELECT * FROM fk_ryow_child ORDER BY id;

-- Multi-row: insert several parents then several children in one txn.
BEGIN;
INSERT INTO fk_ryow_parent VALUES (2), (3), (4);
INSERT INTO fk_ryow_child VALUES (2, 2), (3, 3), (4, 4);
COMMIT;

SELECT * FROM fk_ryow_child ORDER BY id;

-- Cross-txn FK still works (parent committed in a prior txn).
INSERT INTO fk_ryow_child VALUES (5, 1);
SELECT * FROM fk_ryow_child ORDER BY id;

-- FK violation still caught for non-existent parent.
INSERT INTO fk_ryow_child VALUES (99, 999);

DROP TABLE fk_ryow_child;
DROP TABLE fk_ryow_parent;

-- ================================================================
-- Composite PK: same-txn INSERT parent + child with multi-column PK.
-- Exercises Phase 2 buffer check with encode_pk_values on composite keys.
-- ================================================================

CREATE TABLE fk_ryow_cpk_parent (a INT, b INT, PRIMARY KEY (a, b));
CREATE TABLE fk_ryow_cpk_child (id INT PRIMARY KEY, pa INT, pb INT,
    FOREIGN KEY (pa, pb) REFERENCES fk_ryow_cpk_parent(a, b));

BEGIN;
INSERT INTO fk_ryow_cpk_parent VALUES (1, 10), (2, 20);
INSERT INTO fk_ryow_cpk_child VALUES (1, 1, 10), (2, 2, 20);
COMMIT;

SELECT * FROM fk_ryow_cpk_child ORDER BY id;

DROP TABLE fk_ryow_cpk_child;
DROP TABLE fk_ryow_cpk_parent;

-- ================================================================
-- Same-txn INSERT parent + UPDATE child to reference new parent.
-- UPDATE also calls check_and_lock_pk_keys via validate_foreign_keys_with_cache.
-- ================================================================

CREATE TABLE fk_ryow_upd_parent (id INT PRIMARY KEY);
CREATE TABLE fk_ryow_upd_child (id INT PRIMARY KEY, pid INT REFERENCES fk_ryow_upd_parent(id));

INSERT INTO fk_ryow_upd_parent VALUES (1);
INSERT INTO fk_ryow_upd_child VALUES (1, 1);

BEGIN;
INSERT INTO fk_ryow_upd_parent VALUES (2);
UPDATE fk_ryow_upd_child SET pid = 2 WHERE id = 1;
COMMIT;

SELECT * FROM fk_ryow_upd_child ORDER BY id;

DROP TABLE fk_ryow_upd_child;
DROP TABLE fk_ryow_upd_parent;

-- ================================================================
-- UNIQUE column FK reference: same-txn INSERT parent + child.
-- FK references a UNIQUE column (not PK) — different lookup path.
-- ================================================================

CREATE TABLE fk_ryow_uniq_parent (id INT PRIMARY KEY, code TEXT UNIQUE);
CREATE TABLE fk_ryow_uniq_child (id INT PRIMARY KEY, pcode TEXT REFERENCES fk_ryow_uniq_parent(code));

BEGIN;
INSERT INTO fk_ryow_uniq_parent VALUES (1, 'alpha'), (2, 'beta');
INSERT INTO fk_ryow_uniq_child VALUES (1, 'alpha'), (2, 'beta');
COMMIT;

SELECT * FROM fk_ryow_uniq_child ORDER BY id;

-- FK violation on UNIQUE ref for non-existent value.
INSERT INTO fk_ryow_uniq_child VALUES (99, 'missing');

DROP TABLE fk_ryow_uniq_child;
DROP TABLE fk_ryow_uniq_parent;

-- ================================================================
-- NULL FK value: allowed per PG semantics (no parent required).
-- ================================================================

CREATE TABLE fk_ryow_null_parent (id INT PRIMARY KEY);
CREATE TABLE fk_ryow_null_child (id INT PRIMARY KEY, pid INT REFERENCES fk_ryow_null_parent(id));

BEGIN;
INSERT INTO fk_ryow_null_child VALUES (1, NULL);
COMMIT;

SELECT * FROM fk_ryow_null_child ORDER BY id;

DROP TABLE fk_ryow_null_child;
DROP TABLE fk_ryow_null_parent;
