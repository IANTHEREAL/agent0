-- Issue #1445: deferred LIMIT/OFFSET paths (FOR UPDATE / SKIP LOCKED)
-- must treat NULL as ALL/no-bound, not as 0.

DROP TABLE IF EXISTS for_update_limit_null_1445;
CREATE TABLE for_update_limit_null_1445 (
    id INT PRIMARY KEY,
    type_name TEXT NOT NULL
);

INSERT INTO for_update_limit_null_1445 (id, type_name)
VALUES
    (1, 'int8'),
    (2, 'int4'),
    (3, 'int2'),
    (4, 'text'),
    (5, 'boolean');

-- 1) LIMIT NULL FOR UPDATE -> all rows.
SELECT id FROM for_update_limit_null_1445 ORDER BY id LIMIT NULL FOR UPDATE;

-- 2) OFFSET NULL FOR UPDATE -> all rows (offset 0).
SELECT id FROM for_update_limit_null_1445 ORDER BY id OFFSET NULL FOR UPDATE;

-- 3) LIMIT NULL OFFSET NULL FOR UPDATE -> all rows.
SELECT id FROM for_update_limit_null_1445 ORDER BY id LIMIT NULL OFFSET NULL FOR UPDATE;

-- 4) LIMIT NULL OFFSET 2 FOR UPDATE -> skip first 2 rows.
SELECT id FROM for_update_limit_null_1445 ORDER BY id LIMIT NULL OFFSET 2 FOR UPDATE;

-- 5) LIMIT 2 OFFSET NULL FOR UPDATE -> first 2 rows.
SELECT id FROM for_update_limit_null_1445 ORDER BY id LIMIT 2 OFFSET NULL FOR UPDATE;

-- 6) Deferred ORDER BY path (catalog-dependent ORDER BY expression).
SELECT id, type_name
FROM for_update_limit_null_1445
ORDER BY to_regtype(type_name || '_missing'), id
LIMIT NULL
FOR UPDATE;

-- 7) Sanity: LIMIT 0 remains 0 rows.
SELECT id FROM for_update_limit_null_1445 ORDER BY id LIMIT 0 FOR UPDATE;

-- 8) SKIP LOCKED + LIMIT NULL -> all currently lockable rows.
SELECT id FROM for_update_limit_null_1445 ORDER BY id LIMIT NULL FOR UPDATE SKIP LOCKED;

-- 9) SKIP LOCKED + OFFSET NULL -> all currently lockable rows.
SELECT id FROM for_update_limit_null_1445 ORDER BY id OFFSET NULL FOR UPDATE SKIP LOCKED;

-- 10) Nested cast NULL in deferred path should still mean ALL.
SELECT id FROM for_update_limit_null_1445 ORDER BY id LIMIT NULL::int8::int8 FOR UPDATE;

-- 11) JOIN root + FOR UPDATE should still honor deferred LIMIT.
DROP TABLE IF EXISTS for_update_join_limit_t1_1510;
DROP TABLE IF EXISTS for_update_join_limit_t2_1510;
CREATE TABLE for_update_join_limit_t1_1510 (id INT PRIMARY KEY);
CREATE TABLE for_update_join_limit_t2_1510 (id INT PRIMARY KEY);
INSERT INTO for_update_join_limit_t1_1510 (id) VALUES (1), (2), (3), (4), (5);
INSERT INTO for_update_join_limit_t2_1510 (id) VALUES (1), (2), (3), (4), (5);
SELECT j1.id
FROM for_update_join_limit_t1_1510 j1
JOIN for_update_join_limit_t2_1510 j2 ON j1.id = j2.id
ORDER BY j1.id
LIMIT 2
FOR UPDATE;
DROP TABLE for_update_join_limit_t2_1510;
DROP TABLE for_update_join_limit_t1_1510;

DROP TABLE for_update_limit_null_1445;
