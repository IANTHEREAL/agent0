-- FK NULL semantics (MATCH SIMPLE) + referenced-key validation coverage

DROP TABLE IF EXISTS fk924f_child CASCADE;
DROP TABLE IF EXISTS fk924f_parent CASCADE;
DROP TABLE IF EXISTS fk924e_child CASCADE;
DROP TABLE IF EXISTS fk924e_parent CASCADE;
DROP TABLE IF EXISTS fk924d_child CASCADE;
DROP TABLE IF EXISTS fk924d_parent CASCADE;
DROP TABLE IF EXISTS fk924c_child CASCADE;
DROP TABLE IF EXISTS fk924c_parent CASCADE;
DROP TABLE IF EXISTS fk924b_child CASCADE;
DROP TABLE IF EXISTS fk924b_parent CASCADE;
DROP TABLE IF EXISTS fk924_child CASCADE;
DROP TABLE IF EXISTS fk924_parent CASCADE;
DROP TABLE IF EXISTS fk_self_restrict_all CASCADE;
DROP TABLE IF EXISTS fk_self_noaction_all CASCADE;
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
DROP TABLE IF EXISTS fk_self_insert_pk CASCADE;
DROP TABLE IF EXISTS fk_self_insert_unique CASCADE;
DROP TABLE IF EXISTS fk_self_update_pk CASCADE;
DROP TABLE IF EXISTS fk_self_composite CASCADE;
DROP TABLE IF EXISTS fk_self_insert_other CASCADE;
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

-- 18) Self-referencing INSERT via PK
CREATE TABLE fk_self_insert_pk (
    id INT PRIMARY KEY,
    parent_id INT REFERENCES fk_self_insert_pk(id)
);
INSERT INTO fk_self_insert_pk VALUES (1, 1);   -- self-ref, OK
INSERT INTO fk_self_insert_pk VALUES (2, 99);  -- non-existent, ERROR
SELECT id, parent_id FROM fk_self_insert_pk ORDER BY id;

-- 19) Self-referencing INSERT via UNIQUE column
CREATE TABLE fk_self_insert_unique (
    id INT PRIMARY KEY,
    code TEXT UNIQUE,
    ref_code TEXT REFERENCES fk_self_insert_unique(code)
);
INSERT INTO fk_self_insert_unique VALUES (1, 'A', 'A');  -- self-ref via UNIQUE, OK
INSERT INTO fk_self_insert_unique VALUES (2, 'B', 'Z');  -- non-existent, ERROR
SELECT id, code, ref_code FROM fk_self_insert_unique ORDER BY id;

-- 20) UPDATE changing PK + FK to self-reference
CREATE TABLE fk_self_update_pk (
    id INT PRIMARY KEY,
    parent_id INT REFERENCES fk_self_update_pk(id)
);
INSERT INTO fk_self_update_pk VALUES (1, NULL);
UPDATE fk_self_update_pk SET id = 2, parent_id = 2 WHERE id = 1;  -- self-ref, OK
SELECT id, parent_id FROM fk_self_update_pk ORDER BY id;

-- 21) Negative UPDATE: non-self same-table reference must fail
INSERT INTO fk_self_update_pk VALUES (3, NULL);
UPDATE fk_self_update_pk SET id = 4, parent_id = 2 WHERE id = 3;  -- refs id=2 (exists), OK
UPDATE fk_self_update_pk SET id = 5, parent_id = 99 WHERE id = 4; -- refs id=99 (missing), ERROR
SELECT id, parent_id FROM fk_self_update_pk ORDER BY id;

-- 22) Composite FK self-reference
CREATE TABLE fk_self_composite (
    a INT, b INT,
    ref_a INT, ref_b INT,
    PRIMARY KEY (a, b),
    FOREIGN KEY (ref_a, ref_b) REFERENCES fk_self_composite(a, b)
);
INSERT INTO fk_self_composite VALUES (1, 1, 1, 1);  -- self-ref, OK
INSERT INTO fk_self_composite VALUES (2, 2, 9, 9);  -- non-existent, ERROR
SELECT a, b, ref_a, ref_b FROM fk_self_composite ORDER BY a, b;

-- 23) Non-self INSERT still fails correctly
CREATE TABLE fk_self_insert_other (
    id INT PRIMARY KEY,
    parent_id INT REFERENCES fk_self_insert_other(id)
);
INSERT INTO fk_self_insert_other VALUES (1, NULL);
INSERT INTO fk_self_insert_other VALUES (2, 1);   -- refs existing row, OK
INSERT INTO fk_self_insert_other VALUES (3, 5);   -- refs non-existent, ERROR
SELECT id, parent_id FROM fk_self_insert_other ORDER BY id;

-- 24) Statement-level NO ACTION: DELETE all rows from self-ref table succeeds
DROP TABLE IF EXISTS fk_self_noaction_all CASCADE;
CREATE TABLE fk_self_noaction_all (
    id INT PRIMARY KEY,
    parent_id INT REFERENCES fk_self_noaction_all(id)
);
INSERT INTO fk_self_noaction_all VALUES (1, NULL), (2, 1), (3, 2);
DELETE FROM fk_self_noaction_all;
SELECT id, parent_id FROM fk_self_noaction_all ORDER BY id;

-- 25) Statement-level NO ACTION: partial delete leaving dangling ref fails
INSERT INTO fk_self_noaction_all VALUES (1, NULL), (2, 1), (3, 2);
DELETE FROM fk_self_noaction_all WHERE id IN (1, 3);
SELECT id, parent_id FROM fk_self_noaction_all ORDER BY id;

-- 26) Statement-level NO ACTION: delete subset where all refs within deleted set
DELETE FROM fk_self_noaction_all;
INSERT INTO fk_self_noaction_all VALUES (1, NULL), (2, 1), (3, NULL);
DELETE FROM fk_self_noaction_all WHERE id IN (1, 2);
SELECT id, parent_id FROM fk_self_noaction_all ORDER BY id;

-- 27) Statement-level RESTRICT: DELETE all rows from self-ref table succeeds (PG parity)
DROP TABLE IF EXISTS fk_self_restrict_all CASCADE;
CREATE TABLE fk_self_restrict_all (
    id INT PRIMARY KEY,
    parent_id INT REFERENCES fk_self_restrict_all(id) ON DELETE RESTRICT
);
INSERT INTO fk_self_restrict_all VALUES (1, NULL), (2, 1), (3, 2);
DELETE FROM fk_self_restrict_all;
SELECT id, parent_id FROM fk_self_restrict_all ORDER BY id;

-- 28) Bug #924: cross-FK stale snapshot – two FKs on same child, same parent, ON DELETE SET NULL
CREATE TABLE fk924_parent (id INT PRIMARY KEY);
CREATE TABLE fk924_child (
    id INT PRIMARY KEY,
    a INT REFERENCES fk924_parent(id) ON DELETE SET NULL,
    b INT REFERENCES fk924_parent(id) ON DELETE SET NULL
);
INSERT INTO fk924_parent VALUES (1);
INSERT INTO fk924_child VALUES (10, 1, 1);
DELETE FROM fk924_parent WHERE id = 1;
SELECT id, a, b FROM fk924_child WHERE id = 10;

-- 29) Bug #924 variant: three FKs on same child, same parent, ON DELETE SET NULL
CREATE TABLE fk924b_parent (id INT PRIMARY KEY);
CREATE TABLE fk924b_child (
    id INT PRIMARY KEY,
    a INT REFERENCES fk924b_parent(id) ON DELETE SET NULL,
    b INT REFERENCES fk924b_parent(id) ON DELETE SET NULL,
    c INT REFERENCES fk924b_parent(id) ON DELETE SET NULL
);
INSERT INTO fk924b_parent VALUES (1);
INSERT INTO fk924b_child VALUES (10, 1, 1, 1);
DELETE FROM fk924b_parent WHERE id = 1;
SELECT id, a, b, c FROM fk924b_child WHERE id = 10;

-- 30) Bug #924 variant: two FKs on same child, same parent, ON DELETE SET DEFAULT
CREATE TABLE fk924c_parent (id INT PRIMARY KEY);
CREATE TABLE fk924c_child (
    id INT PRIMARY KEY,
    a INT DEFAULT 0 REFERENCES fk924c_parent(id) ON DELETE SET DEFAULT,
    b INT DEFAULT 0 REFERENCES fk924c_parent(id) ON DELETE SET DEFAULT
);
INSERT INTO fk924c_parent VALUES (0), (1);
INSERT INTO fk924c_child VALUES (10, 1, 1);
DELETE FROM fk924c_parent WHERE id = 1;
SELECT id, a, b FROM fk924c_child WHERE id = 10;

-- 31) Bug #924 variant: mixed SET NULL + SET DEFAULT on same child row
CREATE TABLE fk924d_parent (id INT PRIMARY KEY);
CREATE TABLE fk924d_child (
    id INT PRIMARY KEY,
    a INT REFERENCES fk924d_parent(id) ON DELETE SET NULL,
    b INT DEFAULT 0 REFERENCES fk924d_parent(id) ON DELETE SET DEFAULT
);
INSERT INTO fk924d_parent VALUES (0), (1);
INSERT INTO fk924d_child VALUES (10, 1, 1);
DELETE FROM fk924d_parent WHERE id = 1;
SELECT id, a, b FROM fk924d_child WHERE id = 10;

-- 32) Bug #924: intra-batch staleness via ON UPDATE CASCADE side-effect
CREATE TABLE fk924e_parent (id INT PRIMARY KEY);
CREATE TABLE fk924e_child (
    id INT PRIMARY KEY,
    a INT,
    b INT,
    ref_a INT,
    ref_b INT,
    UNIQUE (a, b),
    FOREIGN KEY (a) REFERENCES fk924e_parent(id) ON DELETE SET NULL,
    FOREIGN KEY (ref_a, ref_b) REFERENCES fk924e_child(a, b) ON UPDATE CASCADE
);
INSERT INTO fk924e_parent VALUES (1);
INSERT INTO fk924e_child VALUES (1, 1, 10, NULL, NULL);
INSERT INTO fk924e_child VALUES (2, 1, 20, 1, 10);
DELETE FROM fk924e_parent WHERE id = 1;
SELECT id, a, b, ref_a, ref_b FROM fk924e_child ORDER BY id;

-- 33) Bug #924: SET DEFAULT where default equals deleted parent key
CREATE TABLE fk924f_parent (id INT PRIMARY KEY);
CREATE TABLE fk924f_child (
    id INT PRIMARY KEY,
    pid INT DEFAULT 1 REFERENCES fk924f_parent(id) ON DELETE SET DEFAULT
);
INSERT INTO fk924f_parent VALUES (1);
INSERT INTO fk924f_child VALUES (1, 1);
DELETE FROM fk924f_parent WHERE id = 1;
SELECT id, pid FROM fk924f_child ORDER BY id;
