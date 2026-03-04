-- Multi-FK ON UPDATE: mixed actions on same parent (#1331)
-- Covers: CASCADE+SET NULL, CASCADE+RESTRICT, CASCADE+SET DEFAULT,
--         and row matching only the second FK rule.

-- 1) CASCADE + SET NULL (two FKs to same parent, different actions)
CREATE TABLE mfk1_parent (id INT PRIMARY KEY);
CREATE TABLE mfk1_child (
    cid INT PRIMARY KEY,
    fk_a INT REFERENCES mfk1_parent(id) ON UPDATE CASCADE,
    fk_b INT REFERENCES mfk1_parent(id) ON UPDATE SET NULL
);
INSERT INTO mfk1_parent VALUES (1);
INSERT INTO mfk1_child VALUES (1, 1, 1);
UPDATE mfk1_parent SET id = 2 WHERE id = 1;
SELECT cid, fk_a, fk_b FROM mfk1_child ORDER BY cid;

-- 2) CASCADE + RESTRICT (RESTRICT must block the entire update)
CREATE TABLE mfk2_parent (id INT PRIMARY KEY);
CREATE TABLE mfk2_child (
    cid INT PRIMARY KEY,
    fk_a INT REFERENCES mfk2_parent(id) ON UPDATE CASCADE,
    fk_b INT REFERENCES mfk2_parent(id) ON UPDATE RESTRICT
);
INSERT INTO mfk2_parent VALUES (1);
INSERT INTO mfk2_child VALUES (1, 1, 1);
UPDATE mfk2_parent SET id = 2 WHERE id = 1;
-- parent must be unchanged after RESTRICT error
SELECT id FROM mfk2_parent ORDER BY id;

-- 3) CASCADE + SET DEFAULT (two FKs, different actions)
CREATE TABLE mfk3_parent (id INT PRIMARY KEY);
INSERT INTO mfk3_parent VALUES (0), (1);
CREATE TABLE mfk3_child (
    cid INT PRIMARY KEY,
    fk_a INT REFERENCES mfk3_parent(id) ON UPDATE CASCADE,
    fk_b INT DEFAULT 0 REFERENCES mfk3_parent(id) ON UPDATE SET DEFAULT
);
INSERT INTO mfk3_child VALUES (1, 1, 1);
UPDATE mfk3_parent SET id = 2 WHERE id = 1;
SELECT cid, fk_a, fk_b FROM mfk3_child ORDER BY cid;

-- 4) Row matches only rule #2, not rule #1
--    Parent changes both id and code; child row refs only code (pid refs a different parent row).
CREATE TABLE mfk4_parent (id INT PRIMARY KEY, code TEXT UNIQUE);
INSERT INTO mfk4_parent VALUES (1, 'A'), (2, 'B');
CREATE TABLE mfk4_child (
    cid INT PRIMARY KEY,
    pid INT REFERENCES mfk4_parent(id) ON UPDATE CASCADE,
    pcode TEXT REFERENCES mfk4_parent(code) ON UPDATE CASCADE
);
INSERT INTO mfk4_child VALUES (1, 2, 'A');
UPDATE mfk4_parent SET id = 10, code = 'Z' WHERE id = 1;
SELECT cid, pid, pcode FROM mfk4_child ORDER BY cid;

-- cleanup
DROP TABLE mfk4_child;
DROP TABLE mfk4_parent;
DROP TABLE mfk3_child;
DROP TABLE mfk3_parent;
DROP TABLE mfk2_child;
DROP TABLE mfk2_parent;
DROP TABLE mfk1_child;
DROP TABLE mfk1_parent;
