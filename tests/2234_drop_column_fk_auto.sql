-- Regression test: DROP COLUMN auto-drops FK constraints (#2234/#2235)
-- PostgreSQL auto-drops child-side FK when a referencing column is dropped.
-- Parent-side DROP of referenced column requires CASCADE (or errors without it).

-- ================================================================
-- 1. Child-side single-column FK: drop referencing column → FK auto-dropped
-- ================================================================

CREATE TABLE dcfk_parent1 (id INT PRIMARY KEY);
CREATE TABLE dcfk_child1 (
    id INT PRIMARY KEY,
    pid INT REFERENCES dcfk_parent1(id)
);

INSERT INTO dcfk_parent1 VALUES (1);
INSERT INTO dcfk_child1 VALUES (1, 1);

-- Drop the FK referencing column — should succeed, FK silently dropped
ALTER TABLE dcfk_child1 DROP COLUMN pid;

-- Verify column is gone
SELECT id FROM dcfk_child1 ORDER BY id;

DROP TABLE dcfk_child1;
DROP TABLE dcfk_parent1;

-- ================================================================
-- 2. Child-side multi-column FK: drop one referencing column → entire FK auto-dropped
-- ================================================================

CREATE TABLE dcfk_parent2 (a INT, b INT, PRIMARY KEY (a, b));
CREATE TABLE dcfk_child2 (
    id INT PRIMARY KEY,
    pa INT,
    pb INT,
    FOREIGN KEY (pa, pb) REFERENCES dcfk_parent2(a, b)
);

INSERT INTO dcfk_parent2 VALUES (1, 10);
INSERT INTO dcfk_child2 VALUES (1, 1, 10);

-- Drop one column of the composite FK → entire FK auto-dropped
ALTER TABLE dcfk_child2 DROP COLUMN pa;

-- Verify: INSERT with any pb value should succeed (FK no longer enforced)
INSERT INTO dcfk_child2 (id, pb) VALUES (2, 999);
SELECT id, pb FROM dcfk_child2 ORDER BY id;

DROP TABLE dcfk_child2;
DROP TABLE dcfk_parent2;

-- ================================================================
-- 3. Parent-side: drop referenced column → should ERROR (without CASCADE)
--    (UNIQUE column has a constraint index, so the index check blocks first)
-- ================================================================

CREATE TABLE dcfk_parent3 (id INT PRIMARY KEY, code TEXT UNIQUE);
CREATE TABLE dcfk_child3 (
    id INT PRIMARY KEY,
    pcode TEXT REFERENCES dcfk_parent3(code)
);

-- This should ERROR: index blocks drop without CASCADE
ALTER TABLE dcfk_parent3 DROP COLUMN code;

-- Verify FK still exists: violating insert should fail
INSERT INTO dcfk_parent3 VALUES (1, 'A');
INSERT INTO dcfk_child3 VALUES (1, 'MISSING');

DROP TABLE dcfk_child3;
DROP TABLE dcfk_parent3;

-- ================================================================
-- 4. Self-referencing FK: drop referencing column → FK auto-dropped
-- ================================================================

CREATE TABLE dcfk_self4 (
    id INT PRIMARY KEY,
    parent_id INT REFERENCES dcfk_self4(id)
);

INSERT INTO dcfk_self4 VALUES (1, NULL), (2, 1);

ALTER TABLE dcfk_self4 DROP COLUMN parent_id;

SELECT id FROM dcfk_self4 ORDER BY id;

DROP TABLE dcfk_self4;

-- ================================================================
-- 5. Multiple FKs from different tables: child-side drops work independently
-- ================================================================

CREATE TABLE dcfk_parent5 (id INT PRIMARY KEY);
CREATE TABLE dcfk_child5a (
    id INT PRIMARY KEY,
    pid INT REFERENCES dcfk_parent5(id)
);
CREATE TABLE dcfk_child5b (
    id INT PRIMARY KEY,
    pid INT REFERENCES dcfk_parent5(id)
);

-- Dropping child-side FK column works for each child independently
ALTER TABLE dcfk_child5a DROP COLUMN pid;
SELECT id FROM dcfk_child5a ORDER BY id;

-- child5b FK still enforced
INSERT INTO dcfk_parent5 VALUES (1);
INSERT INTO dcfk_child5b VALUES (1, 999);

DROP TABLE dcfk_child5b;
DROP TABLE dcfk_child5a;
DROP TABLE dcfk_parent5;

-- ================================================================
-- 6. Behavioral verification: FK violation before drop, success after drop
-- ================================================================

CREATE TABLE dcfk_parent6 (id INT PRIMARY KEY);
CREATE TABLE dcfk_child6 (
    id INT PRIMARY KEY,
    pid INT REFERENCES dcfk_parent6(id),
    extra TEXT
);

INSERT INTO dcfk_parent6 VALUES (1);
INSERT INTO dcfk_child6 VALUES (1, 1, 'ok');

-- This should fail: FK violation
INSERT INTO dcfk_child6 VALUES (2, 999, 'should fail');

-- Now drop the FK column
ALTER TABLE dcfk_child6 DROP COLUMN pid;

-- This INSERT should succeed (FK is gone)
INSERT INTO dcfk_child6 (id, extra) VALUES (3, 'no fk check');
SELECT id, extra FROM dcfk_child6 ORDER BY id;

DROP TABLE dcfk_child6;
DROP TABLE dcfk_parent6;

-- ================================================================
-- 7. Non-FK column drop: FK still enforced after unrelated column drop
-- ================================================================

CREATE TABLE dcfk_parent7 (id INT PRIMARY KEY);
CREATE TABLE dcfk_child7 (
    id INT PRIMARY KEY,
    pid INT REFERENCES dcfk_parent7(id),
    notes TEXT
);

INSERT INTO dcfk_parent7 VALUES (1);
INSERT INTO dcfk_child7 VALUES (1, 1, 'hello');

-- Drop unrelated column
ALTER TABLE dcfk_child7 DROP COLUMN notes;

-- FK must still be enforced
INSERT INTO dcfk_child7 VALUES (2, 999);

-- Valid FK insert works
INSERT INTO dcfk_child7 VALUES (2, 1);
SELECT id, pid FROM dcfk_child7 ORDER BY id;

DROP TABLE dcfk_child7;
DROP TABLE dcfk_parent7;

-- ================================================================
-- 8. CASCADE: drop parent column CASCADE → auto-drops child FK + UNIQUE index
-- ================================================================

CREATE TABLE dcfk_parent8 (id INT PRIMARY KEY, code TEXT UNIQUE);
CREATE TABLE dcfk_child8 (
    id INT PRIMARY KEY,
    pcode TEXT REFERENCES dcfk_parent8(code)
);

INSERT INTO dcfk_parent8 VALUES (1, 'A');
INSERT INTO dcfk_child8 VALUES (1, 'A');

-- CASCADE drops the UNIQUE index and the child FK constraint
ALTER TABLE dcfk_parent8 DROP COLUMN code CASCADE;

-- After CASCADE, child table still exists but FK is gone
INSERT INTO dcfk_child8 VALUES (2, 'ANYTHING');
SELECT id, pcode FROM dcfk_child8 ORDER BY id;

DROP TABLE dcfk_child8;
DROP TABLE dcfk_parent8;

-- ================================================================
-- 9. IF EXISTS on non-existent column: no-op, FK untouched
-- ================================================================

CREATE TABLE dcfk_parent9 (id INT PRIMARY KEY);
CREATE TABLE dcfk_child9 (
    id INT PRIMARY KEY,
    pid INT REFERENCES dcfk_parent9(id)
);

-- Drop non-existent column with IF EXISTS — no-op
ALTER TABLE dcfk_child9 DROP COLUMN IF EXISTS nonexistent;

-- FK must still be enforced
INSERT INTO dcfk_parent9 VALUES (1);
INSERT INTO dcfk_child9 VALUES (1, 999);

DROP TABLE dcfk_child9;
DROP TABLE dcfk_parent9;
