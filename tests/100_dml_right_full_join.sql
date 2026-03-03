-- Regression test for #1248: RIGHT/FULL OUTER JOIN in DML FROM/USING
-- must not silently degrade to INNER JOIN.

-- ============================================
-- 1. UPDATE ... FROM with RIGHT JOIN
-- ============================================
DROP TABLE IF EXISTS rj_target CASCADE;
DROP TABLE IF EXISTS rj_left CASCADE;
DROP TABLE IF EXISTS rj_right CASCADE;

CREATE TABLE rj_target (id INT PRIMARY KEY, val TEXT);
CREATE TABLE rj_left (id INT PRIMARY KEY, lval INT);
CREATE TABLE rj_right (id INT PRIMARY KEY, rval INT);

INSERT INTO rj_target VALUES (1, 'a'), (2, 'b'), (3, 'c');
INSERT INTO rj_left VALUES (1, 10), (2, 20);
INSERT INTO rj_right VALUES (2, 200), (3, 300), (4, 400);

-- RIGHT JOIN produces: (2,20,2,200), (NULL,NULL,3,300), (NULL,NULL,4,400)
-- WHERE t.id = r.id matches target ids 2 and 3 (id=4 has no target row).
-- With INNER JOIN this would only match id=2 — missing the NULL-left id=3 row.
UPDATE rj_target t
SET val = COALESCE(l.lval::text, 'NULL') || '_' || r.rval::text
FROM rj_left l RIGHT JOIN rj_right r ON l.id = r.id
WHERE t.id = r.id;

SELECT id, val FROM rj_target ORDER BY id;

DROP TABLE rj_target;
DROP TABLE rj_left;
DROP TABLE rj_right;

-- ============================================
-- 2. DELETE ... USING with FULL OUTER JOIN
-- ============================================
DROP TABLE IF EXISTS del_target CASCADE;
DROP TABLE IF EXISTS del_a CASCADE;
DROP TABLE IF EXISTS del_b CASCADE;

CREATE TABLE del_target (id INT PRIMARY KEY, val TEXT);
CREATE TABLE del_a (id INT PRIMARY KEY, aval INT);
CREATE TABLE del_b (id INT PRIMARY KEY, bval INT);

INSERT INTO del_target VALUES (1, 'one'), (2, 'two'), (3, 'three'), (4, 'four');
INSERT INTO del_a VALUES (1, 10), (2, 20);
INSERT INTO del_b VALUES (2, 200), (3, 300);

-- FULL JOIN produces: (1,10,NULL,NULL), (2,20,2,200), (NULL,NULL,3,300)
-- WHERE t.id = COALESCE(a.id, b.id) matches target ids 1, 2, 3.
-- With INNER JOIN this would only match id=2 — missing ids 1 and 3.
DELETE FROM del_target t
USING del_a a FULL OUTER JOIN del_b b ON a.id = b.id
WHERE t.id = COALESCE(a.id, b.id);

SELECT id, val FROM del_target ORDER BY id;

DROP TABLE del_target;
DROP TABLE del_a;
DROP TABLE del_b;
