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
DROP TABLE IF EXISTS fk_alter_uniq_child CASCADE;
DROP TABLE IF EXISTS fk_alter_uniq_parent CASCADE;

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

-- 3) DDL rejects FK to UNIQUE non-PK (temporarily gated)
CREATE TABLE fk_parent_unique (
    id INT PRIMARY KEY,
    code TEXT UNIQUE
);

CREATE TABLE fk_child_unique_ref (
    id INT PRIMARY KEY,
    code TEXT REFERENCES fk_parent_unique(code)
);

-- 4) DDL rejects self-referencing FK to UNIQUE non-PK
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

-- 10) ALTER TABLE ADD FK to UNIQUE non-PK is rejected (gated)
CREATE TABLE fk_alter_uniq_parent (
    id INT PRIMARY KEY,
    code TEXT UNIQUE
);

CREATE TABLE fk_alter_uniq_child (
    id INT PRIMARY KEY,
    code TEXT
);

ALTER TABLE fk_alter_uniq_child
ADD CONSTRAINT fk_alter_uniq_child_code_fkey
FOREIGN KEY (code) REFERENCES fk_alter_uniq_parent(code);
