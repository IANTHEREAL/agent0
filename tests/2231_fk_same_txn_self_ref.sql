-- Supplemental regression test: FK same-transaction visibility for self-referencing FK (#2231)
-- The parent test (2182_fk_same_txn_parent_child) covers basic same-txn FK scenarios.
-- This test adds self-referencing FK coverage.

-- ================================================================
-- 1. Self-referencing FK: INSERT root + child in same txn
-- ================================================================

CREATE TABLE fk_txn_tree (
    id INT PRIMARY KEY,
    parent_id INT REFERENCES fk_txn_tree(id)
);

BEGIN;
INSERT INTO fk_txn_tree VALUES (1, NULL);
INSERT INTO fk_txn_tree VALUES (2, 1);
COMMIT;

SELECT id, parent_id FROM fk_txn_tree ORDER BY id;

-- ================================================================
-- 2. Self-referencing FK: row references itself
-- ================================================================

BEGIN;
INSERT INTO fk_txn_tree VALUES (3, 3);
COMMIT;

SELECT id, parent_id FROM fk_txn_tree ORDER BY id;

-- ================================================================
-- 3. Multi-level tree in same txn
-- ================================================================

BEGIN;
INSERT INTO fk_txn_tree VALUES (10, NULL);
INSERT INTO fk_txn_tree VALUES (11, 10);
INSERT INTO fk_txn_tree VALUES (12, 11);
INSERT INTO fk_txn_tree VALUES (13, 12);
COMMIT;

SELECT id, parent_id FROM fk_txn_tree ORDER BY id;

-- ================================================================
-- 4. FK violation: reference non-existent row in same txn
-- ================================================================

BEGIN;
INSERT INTO fk_txn_tree VALUES (20, NULL);
INSERT INTO fk_txn_tree VALUES (21, 999);
COMMIT;

-- Neither row committed (PG aborts entire txn on error)
SELECT id, parent_id FROM fk_txn_tree WHERE id >= 20 ORDER BY id;

-- ================================================================
-- 5. UPDATE to create self-reference in same txn
-- ================================================================

BEGIN;
INSERT INTO fk_txn_tree VALUES (30, NULL);
UPDATE fk_txn_tree SET parent_id = 30 WHERE id = 30;
COMMIT;

SELECT id, parent_id FROM fk_txn_tree WHERE id = 30;

-- ================================================================
-- 6. Composite self-referencing FK in same txn
-- ================================================================

CREATE TABLE fk_txn_comp_tree (
    a INT,
    b INT,
    ref_a INT,
    ref_b INT,
    PRIMARY KEY (a, b),
    FOREIGN KEY (ref_a, ref_b) REFERENCES fk_txn_comp_tree(a, b)
);

BEGIN;
INSERT INTO fk_txn_comp_tree VALUES (1, 1, NULL, NULL);
INSERT INTO fk_txn_comp_tree VALUES (2, 2, 1, 1);
INSERT INTO fk_txn_comp_tree VALUES (3, 3, 2, 2);
COMMIT;

SELECT a, b, ref_a, ref_b FROM fk_txn_comp_tree ORDER BY a, b;

-- Self-reference in composite FK
BEGIN;
INSERT INTO fk_txn_comp_tree VALUES (4, 4, 4, 4);
COMMIT;

SELECT a, b, ref_a, ref_b FROM fk_txn_comp_tree WHERE a = 4;

DROP TABLE fk_txn_comp_tree;

-- ================================================================
-- 7. Multi-row INSERT with mixed NULL and valid FK refs
-- ================================================================

DELETE FROM fk_txn_tree;

BEGIN;
INSERT INTO fk_txn_tree VALUES (1, NULL), (2, NULL), (3, 1), (4, NULL), (5, 3);
COMMIT;

SELECT id, parent_id FROM fk_txn_tree ORDER BY id;

-- ================================================================
-- Cleanup
-- ================================================================

DROP TABLE fk_txn_tree;
