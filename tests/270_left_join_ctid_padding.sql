-- LEFT JOIN unmatched-row ctid padding regression test (#1319/#1335)
-- Verifies that empty-right fallback width is correct for both
-- base-table right operands (has synthetic ctid) and subquery right
-- operands (no ctid).

-- ============================================
-- Setup
-- ============================================
CREATE TABLE lj_left (id INT PRIMARY KEY, val TEXT);
CREATE TABLE lj_right (id INT PRIMARY KEY, left_id INT, info TEXT);

INSERT INTO lj_left VALUES (1, 'a'), (2, 'b'), (3, 'c');
-- Only insert rows matching left_id=1; ids 2,3 will be unmatched.
INSERT INTO lj_right VALUES (10, 1, 'matched');

-- ============================================
-- Test 1: LEFT JOIN with base table right side (unmatched rows)
-- The right side is a base table → runtime row includes synthetic ctid,
-- so the fallback width must be schema.columns.len() + 1.
-- ============================================
SELECT l.id, l.val, r.id AS r_id, r.info
FROM lj_left l LEFT JOIN lj_right r ON l.id = r.left_id
ORDER BY l.id;

-- ============================================
-- Test 2: LEFT JOIN with empty subquery right side (no ctid)
-- The right side is a derived table → no synthetic ctid appended,
-- so the fallback width must be exactly schema.columns.len().
-- ============================================
SELECT l.id, l.val, sub.r_id, sub.info
FROM lj_left l LEFT JOIN (
    SELECT id AS r_id, left_id, info FROM lj_right WHERE left_id = 999
) sub ON l.id = sub.left_id
ORDER BY l.id;

-- ============================================
-- Test 3: UPDATE ... FROM with LEFT JOIN (base table, unmatched)
-- Exercises the dml_analyzed join path directly.
-- ============================================
CREATE TABLE lj_target (id INT PRIMARY KEY, status TEXT);
INSERT INTO lj_target VALUES (1, 'init'), (2, 'init'), (3, 'init');

UPDATE lj_target t
SET status = COALESCE(r.info, 'no_match')
FROM lj_left l LEFT JOIN lj_right r ON l.id = r.left_id
WHERE t.id = l.id;

SELECT id, status FROM lj_target ORDER BY id;

-- ============================================
-- Test 4: UPDATE ... FROM with LEFT JOIN (nested join right side, empty)
-- Right side is (b JOIN c ON ...) — a Join kind with two base-table leaves,
-- each contributing a ctid slot. When the nested join returns no rows,
-- the empty-side fallback must include ctid slots from both leaves.
-- ============================================
CREATE TABLE lj_b (id INT PRIMARY KEY, data TEXT);
CREATE TABLE lj_c (id INT PRIMARY KEY, b_id INT, extra TEXT);
-- Leave both tables empty so the inner join produces no rows.

UPDATE lj_target t
SET status = COALESCE(b.data, 'empty_join')
FROM lj_left l LEFT JOIN (lj_b b JOIN lj_c c ON b.id = c.b_id) ON l.id = b.id
WHERE t.id = l.id;

SELECT id, status FROM lj_target ORDER BY id;

-- ============================================
-- Cleanup
-- ============================================
DROP TABLE lj_c;
DROP TABLE lj_b;
DROP TABLE lj_target;
DROP TABLE lj_right;
DROP TABLE lj_left;
