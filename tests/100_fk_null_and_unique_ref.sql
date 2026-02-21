-- FK NULL semantics (MATCH SIMPLE) + referenced-key validation coverage

DROP TABLE IF EXISTS fk_child_cascade CASCADE;
DROP TABLE IF EXISTS fk_child_restrict CASCADE;
DROP TABLE IF EXISTS fk_parent_del CASCADE;
DROP TABLE IF EXISTS fk_alter_child CASCADE;
DROP TABLE IF EXISTS fk_alter_parent CASCADE;
DROP TABLE IF EXISTS fk_child_omit CASCADE;
DROP TABLE IF EXISTS fk_parent_omit CASCADE;
DROP TABLE IF EXISTS fk_parent_no_pk CASCADE;
DROP TABLE IF EXISTS fk_parent_arity2 CASCADE;
DROP TABLE IF EXISTS fk_parent_arity1 CASCADE;
DROP TABLE IF EXISTS fk_child_arity_mismatch_2 CASCADE;
DROP TABLE IF EXISTS fk_child_arity_mismatch_1 CASCADE;
DROP TABLE IF EXISTS fk_child_no_pk CASCADE;
DROP TABLE IF EXISTS fk_child_unique_ref CASCADE;
DROP TABLE IF EXISTS fk_parent_unique CASCADE;
DROP TABLE IF EXISTS fk_child_pk CASCADE;
DROP TABLE IF EXISTS fk_parent_pk CASCADE;
DROP TABLE IF EXISTS fk_child_cmp CASCADE;
DROP TABLE IF EXISTS fk_parent_cmp CASCADE;
DROP TABLE IF EXISTS fk_self_unique CASCADE;
DROP TABLE IF EXISTS fk_self_restrict CASCADE;
DROP TABLE IF EXISTS fk_self_del_cascade CASCADE;
DROP TABLE IF EXISTS fk_self_upd_cascade CASCADE;
DROP TABLE IF EXISTS fk_self_row_cascade CASCADE;
DROP TABLE IF EXISTS fk_self_row_restrict CASCADE;
DROP TABLE IF EXISTS fk_self_cycle CASCADE;
DROP TABLE IF EXISTS fk_alter_uniq_child CASCADE;
DROP TABLE IF EXISTS fk_alter_uniq_parent CASCADE;
DROP TABLE IF EXISTS fk_uref_child CASCADE;
DROP TABLE IF EXISTS fk_uref_parent CASCADE;
DROP TABLE IF EXISTS fk_impl_child CASCADE;
DROP TABLE IF EXISTS fk_impl_parent CASCADE;

-- 1) Composite FK + MATCH SIMPLE (any NULL skips check)
CREATE TABLE fk_parent_cmp (
    a INT,
    b INT,
    PRIMARY KEY (a, b)
);

CREATE TABLE fk_child_cmp (
    id INT PRIMARY KEY,
    a INT,
    b INT,
    FOREIGN KEY (a, b) REFERENCES fk_parent_cmp(a, b)
);

INSERT INTO fk_parent_cmp VALUES (1, 1), (2, 2);
INSERT INTO fk_child_cmp VALUES (1, NULL, 1);
INSERT INTO fk_child_cmp VALUES (2, 1, NULL);
INSERT INTO fk_child_cmp VALUES (3, NULL, NULL);
INSERT INTO fk_child_cmp VALUES (4, 1, 1);
INSERT INTO fk_child_cmp VALUES (5, 9, 99);

SELECT id, a, b FROM fk_child_cmp ORDER BY id;

-- 2) FK to PK regression (valid / invalid / NULL)
CREATE TABLE fk_parent_pk (
    id INT PRIMARY KEY
);

CREATE TABLE fk_child_pk (
    id INT PRIMARY KEY,
    pid INT REFERENCES fk_parent_pk(id)
);

INSERT INTO fk_parent_pk VALUES (10);
INSERT INTO fk_child_pk VALUES (1, 10);
INSERT INTO fk_child_pk VALUES (2, NULL);
INSERT INTO fk_child_pk VALUES (3, 99);

SELECT id, pid FROM fk_child_pk ORDER BY id;

-- 3) FK to UNIQUE non-PK with cascades
CREATE TABLE fk_parent_unique (
    id INT PRIMARY KEY,
    code TEXT UNIQUE
);

CREATE TABLE fk_child_unique_ref (
    id INT PRIMARY KEY,
    code TEXT REFERENCES fk_parent_unique(code) ON DELETE CASCADE ON UPDATE CASCADE
);

INSERT INTO fk_parent_unique VALUES (1, 'A'), (2, 'B');
INSERT INTO fk_child_unique_ref VALUES (1, 'A');
INSERT INTO fk_child_unique_ref VALUES (2, NULL);
INSERT INTO fk_child_unique_ref VALUES (3, 'Z');
SELECT id, code FROM fk_child_unique_ref ORDER BY id;
UPDATE fk_parent_unique SET code = 'AA' WHERE id = 1;
SELECT id, code FROM fk_child_unique_ref ORDER BY id;
DELETE FROM fk_parent_unique WHERE id = 2;
DELETE FROM fk_parent_unique WHERE id = 1;
SELECT id, code FROM fk_child_unique_ref ORDER BY id;

-- 4) Self-referencing FK to UNIQUE non-PK (DDL allowed)
CREATE TABLE fk_self_unique (
    id INT PRIMARY KEY,
    code TEXT UNIQUE,
    ref_code TEXT REFERENCES fk_self_unique(code)
);

-- 5) DDL rejects arity mismatch for omitted referenced-column list
CREATE TABLE fk_parent_arity1 (
    id INT PRIMARY KEY
);

CREATE TABLE fk_child_arity_mismatch_1 (
    a INT,
    b INT,
    FOREIGN KEY (a, b) REFERENCES fk_parent_arity1(id)
);

CREATE TABLE fk_parent_arity2 (
    a INT,
    b INT,
    PRIMARY KEY (a, b)
);

CREATE TABLE fk_child_arity_mismatch_2 (
    a INT REFERENCES fk_parent_arity2(a, b)
);

-- 6) Inline REFERENCES to table without PK -> no unique constraint matching
CREATE TABLE fk_parent_no_pk (
    code INT
);

CREATE TABLE fk_child_no_pk (
    id INT PRIMARY KEY,
    code INT REFERENCES fk_parent_no_pk
);

-- 7) Valid omitted-column FK to PK
CREATE TABLE fk_parent_omit (
    id INT PRIMARY KEY
);

CREATE TABLE fk_child_omit (
    id INT PRIMARY KEY,
    pid INT REFERENCES fk_parent_omit
);

INSERT INTO fk_parent_omit VALUES (1);
INSERT INTO fk_child_omit VALUES (1, 1);
INSERT INTO fk_child_omit VALUES (2, NULL);
INSERT INTO fk_child_omit VALUES (3, 2);

SELECT id, pid FROM fk_child_omit ORDER BY id;

-- 8) ALTER TABLE ADD FK + MATCH SIMPLE null behavior on existing rows
CREATE TABLE fk_alter_parent (
    a INT,
    b INT,
    PRIMARY KEY (a, b)
);

CREATE TABLE fk_alter_child (
    id INT PRIMARY KEY,
    a INT,
    b INT
);

INSERT INTO fk_alter_parent VALUES (1, 1), (2, 2);
INSERT INTO fk_alter_child VALUES (1, 1, 1);
INSERT INTO fk_alter_child VALUES (2, NULL, 1);
INSERT INTO fk_alter_child VALUES (3, 1, NULL);
INSERT INTO fk_alter_child VALUES (4, NULL, NULL);

ALTER TABLE fk_alter_child
ADD CONSTRAINT fk_alter_child_ab_fkey
FOREIGN KEY (a, b) REFERENCES fk_alter_parent(a, b);

INSERT INTO fk_alter_child VALUES (5, 9, 9);
SELECT id, a, b FROM fk_alter_child ORDER BY id;

-- 9) Parent DELETE regression for RESTRICT / CASCADE on PK refs
CREATE TABLE fk_parent_del (
    id INT PRIMARY KEY
);

CREATE TABLE fk_child_restrict (
    id INT PRIMARY KEY,
    pid INT REFERENCES fk_parent_del(id)
);

CREATE TABLE fk_child_cascade (
    id INT PRIMARY KEY,
    pid INT REFERENCES fk_parent_del(id) ON DELETE CASCADE
);

INSERT INTO fk_parent_del VALUES (1), (2);
INSERT INTO fk_child_restrict VALUES (1, 1);
INSERT INTO fk_child_cascade VALUES (1, 1), (2, 2);

DELETE FROM fk_parent_del WHERE id = 1;
DELETE FROM fk_parent_del WHERE id = 2;

SELECT id FROM fk_parent_del ORDER BY id;
SELECT id, pid FROM fk_child_restrict ORDER BY id;
SELECT id, pid FROM fk_child_cascade ORDER BY id;

-- 10) ALTER TABLE ADD FK to UNIQUE non-PK with row validation
CREATE TABLE fk_alter_uniq_parent (
    id INT PRIMARY KEY,
    code TEXT UNIQUE
);

CREATE TABLE fk_alter_uniq_child (
    id INT PRIMARY KEY,
    code TEXT
);

INSERT INTO fk_alter_uniq_parent VALUES (1, 'X'), (2, 'Y');
INSERT INTO fk_alter_uniq_child VALUES (1, 'X');
INSERT INTO fk_alter_uniq_child VALUES (2, 'INVALID');

ALTER TABLE fk_alter_uniq_child
ADD CONSTRAINT fk_alter_uniq_child_code_fkey
FOREIGN KEY (code) REFERENCES fk_alter_uniq_parent(code);

DELETE FROM fk_alter_uniq_child WHERE id = 2;

ALTER TABLE fk_alter_uniq_child
ADD CONSTRAINT fk_alter_uniq_child_code_fkey
FOREIGN KEY (code) REFERENCES fk_alter_uniq_parent(code);

SELECT id, code FROM fk_alter_uniq_child ORDER BY id;

-- 11) Bug #3 regression: PK unchanged, UNIQUE ref changes -> ON UPDATE CASCADE fires
CREATE TABLE fk_uref_parent (
    id INT PRIMARY KEY,
    code TEXT UNIQUE
);

CREATE TABLE fk_uref_child (
    id INT PRIMARY KEY,
    code TEXT REFERENCES fk_uref_parent(code) ON UPDATE CASCADE
);

INSERT INTO fk_uref_parent VALUES (1, 'A');
INSERT INTO fk_uref_child VALUES (1, 'A');
UPDATE fk_uref_parent SET code = 'Z' WHERE id = 1;
SELECT id, code FROM fk_uref_child ORDER BY id;

-- 12) Implicit-PK FK error-detail (omitted ref_columns shows PK names)
CREATE TABLE fk_impl_parent (id INT PRIMARY KEY);

CREATE TABLE fk_impl_child (
    id INT PRIMARY KEY,
    pid INT REFERENCES fk_impl_parent
);

INSERT INTO fk_impl_parent VALUES (1);
INSERT INTO fk_impl_child VALUES (1, 1);
DELETE FROM fk_impl_parent WHERE id = 1;

-- 13) Self-referencing FK DELETE enforcement
CREATE TABLE fk_self_restrict (
    id INT PRIMARY KEY,
    parent_id INT REFERENCES fk_self_restrict(id)
);
INSERT INTO fk_self_restrict VALUES (1, NULL), (2, 1), (3, 2);
DELETE FROM fk_self_restrict WHERE id = 3;  -- leaf, OK
DELETE FROM fk_self_restrict WHERE id = 1;  -- RESTRICT error (id=2 refs id=1)
SELECT id, parent_id FROM fk_self_restrict ORDER BY id;

CREATE TABLE fk_self_del_cascade (
    id INT PRIMARY KEY,
    parent_id INT REFERENCES fk_self_del_cascade(id) ON DELETE CASCADE
);
INSERT INTO fk_self_del_cascade VALUES (1, NULL), (2, 1), (3, 2), (4, 2);
DELETE FROM fk_self_del_cascade WHERE id = 1;  -- cascades: 2→3,4 all deleted
SELECT id, parent_id FROM fk_self_del_cascade ORDER BY id;

-- 14) Self-referencing FK ON UPDATE CASCADE (UNIQUE ref)
CREATE TABLE fk_self_upd_cascade (
    id INT PRIMARY KEY,
    code TEXT UNIQUE,
    ref_code TEXT REFERENCES fk_self_upd_cascade(code) ON UPDATE CASCADE
);
INSERT INTO fk_self_upd_cascade VALUES (1, 'A', NULL), (2, 'B', 'A');
UPDATE fk_self_upd_cascade SET code = 'AA' WHERE id = 1;  -- cascade: id=2 ref_code A→AA
SELECT id, code, ref_code FROM fk_self_upd_cascade ORDER BY id;

-- 15) Self-row UPDATE CASCADE: row's ref_code points to its own code
CREATE TABLE fk_self_row_cascade (
    id INT PRIMARY KEY,
    code TEXT UNIQUE,
    ref_code TEXT REFERENCES fk_self_row_cascade(code) ON UPDATE CASCADE
);
INSERT INTO fk_self_row_cascade VALUES (1, 'A', 'A');
UPDATE fk_self_row_cascade SET code = 'AA' WHERE id = 1;  -- cascade rewrites own ref_code
SELECT id, code, ref_code FROM fk_self_row_cascade ORDER BY id;

-- 16) Self-row UPDATE RESTRICT: row's ref_code points to its own code
CREATE TABLE fk_self_row_restrict (
    id INT PRIMARY KEY,
    code TEXT UNIQUE,
    ref_code TEXT REFERENCES fk_self_row_restrict(code)
);
INSERT INTO fk_self_row_restrict VALUES (1, 'A', 'A');
UPDATE fk_self_row_restrict SET code = 'AA' WHERE id = 1;  -- RESTRICT error
SELECT id, code, ref_code FROM fk_self_row_restrict ORDER BY id;

-- 17) Cyclic self-referencing ON DELETE CASCADE (2-node cycle)
CREATE TABLE fk_self_cycle (
    id INT PRIMARY KEY,
    ref_id INT REFERENCES fk_self_cycle(id) ON DELETE CASCADE
);
INSERT INTO fk_self_cycle VALUES (1, NULL), (2, NULL);
UPDATE fk_self_cycle SET ref_id = 2 WHERE id = 1;
UPDATE fk_self_cycle SET ref_id = 1 WHERE id = 2;
DELETE FROM fk_self_cycle WHERE id = 1;  -- cascades: both rows deleted
SELECT id, ref_id FROM fk_self_cycle ORDER BY id;
