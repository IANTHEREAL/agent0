-- Regression: RIGHT/FULL JOIN where the empty side is a nested join
-- containing base tables. count_ctid_slots must recurse into Join kind
-- to compute correct NULL-padding width (#1269 block 3).

-- ── Setup ──────────────────────────────────────────────────────────
DROP TABLE IF EXISTS njr_a;
DROP TABLE IF EXISTS njr_b;
DROP TABLE IF EXISTS njr_c;

CREATE TABLE njr_a(id INT PRIMARY KEY, val TEXT);
CREATE TABLE njr_b(id INT PRIMARY KEY, val TEXT);
CREATE TABLE njr_c(id INT PRIMARY KEY, val TEXT);

INSERT INTO njr_c VALUES (1, 'c1'), (2, 'c2');

-- ── RIGHT JOIN: empty nested-join on LEFT side ─────────────────────
-- Left side is (njr_a JOIN njr_b) which is empty (both tables empty).
-- Each base table contributes a ctid slot, so empty-side width must
-- account for 2 ctid slots (one per base table in the nested join).
SELECT njr_a.id, njr_a.val, njr_b.id, njr_b.val, njr_c.id, njr_c.val
FROM njr_a
INNER JOIN njr_b ON njr_a.id = njr_b.id
RIGHT JOIN njr_c ON njr_a.id = njr_c.id
ORDER BY njr_c.id;

-- ── FULL JOIN: empty nested-join on LEFT side ──────────────────────
SELECT njr_a.id, njr_a.val, njr_b.id, njr_b.val, njr_c.id, njr_c.val
FROM njr_a
INNER JOIN njr_b ON njr_a.id = njr_b.id
FULL JOIN njr_c ON njr_a.id = njr_c.id
ORDER BY COALESCE(njr_c.id, njr_a.id);

-- ── FULL JOIN: empty nested-join on RIGHT side ─────────────────────
TRUNCATE TABLE njr_c;
INSERT INTO njr_a VALUES (1, 'a1'), (2, 'a2');

SELECT njr_a.id, njr_a.val, njr_b.id, njr_b.val, njr_c.id, njr_c.val
FROM njr_a
FULL JOIN (njr_b INNER JOIN njr_c ON njr_b.id = njr_c.id) ON njr_a.id = njr_b.id
ORDER BY COALESCE(njr_a.id, njr_b.id);

-- ── Cleanup ────────────────────────────────────────────────────────
DROP TABLE njr_a;
DROP TABLE njr_b;
DROP TABLE njr_c;
