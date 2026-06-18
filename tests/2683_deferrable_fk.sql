-- #2683: parser + DDL must accept DEFERRABLE constraint characteristics
-- ([NOT] DEFERRABLE [INITIALLY DEFERRED|IMMEDIATE]) on FK / UNIQUE / PK in
-- every constraint form. Django emits these on every foreign key.
--
-- PG-DIVERGENCE: db9 records condeferrable/condeferred for catalog fidelity but
-- still checks foreign keys immediately, not at COMMIT.

DROP TABLE IF EXISTS defer_named_uniq CASCADE;
DROP TABLE IF EXISTS defer_muc CASCADE;
DROP TABLE IF EXISTS defer_uniq_def CASCADE;
DROP TABLE IF EXISTS defer_pk CASCADE;
DROP TABLE IF EXISTS defer_child_nd CASCADE;
DROP TABLE IF EXISTS defer_uniq CASCADE;
DROP TABLE IF EXISTS defer_child_alter CASCADE;
DROP TABLE IF EXISTS defer_child_tbl CASCADE;
DROP TABLE IF EXISTS defer_child CASCADE;
DROP TABLE IF EXISTS defer_parent CASCADE;

CREATE TABLE defer_parent (id INT PRIMARY KEY);

-- Form 1: inline column REFERENCES ... DEFERRABLE INITIALLY DEFERRED
CREATE TABLE defer_child (
    id INT PRIMARY KEY,
    parent_id INT CONSTRAINT defer_child_fk REFERENCES defer_parent(id) DEFERRABLE INITIALLY DEFERRED
);

-- Form 2: table-level FOREIGN KEY ... DEFERRABLE (defaults INITIALLY IMMEDIATE)
CREATE TABLE defer_child_tbl (
    id INT PRIMARY KEY,
    parent_id INT,
    CONSTRAINT defer_child_tbl_fk FOREIGN KEY (parent_id) REFERENCES defer_parent(id) DEFERRABLE
);

-- Form 3: ALTER TABLE ADD CONSTRAINT ... DEFERRABLE INITIALLY DEFERRED (Django's form)
CREATE TABLE defer_child_alter (
    id INT PRIMARY KEY,
    parent_id INT
);
ALTER TABLE defer_child_alter
    ADD CONSTRAINT defer_child_alter_fk FOREIGN KEY (parent_id)
    REFERENCES defer_parent(id) DEFERRABLE INITIALLY DEFERRED;

-- Form 4: UNIQUE accepts DEFERRABLE too (creation must succeed)
CREATE TABLE defer_uniq (
    id INT PRIMARY KEY,
    code INT,
    CONSTRAINT defer_uniq_u UNIQUE (code) DEFERRABLE INITIALLY IMMEDIATE
);

-- Explicit NOT DEFERRABLE parses (inline, auto-named FK)
CREATE TABLE defer_child_nd (
    id INT PRIMARY KEY,
    parent_id INT REFERENCES defer_parent(id) NOT DEFERRABLE
);

-- Catalog reflects deferrability for FK constraints.
SELECT conname, condeferrable, condeferred
FROM pg_constraint
WHERE contype = 'f' AND conname LIKE 'defer_%'
ORDER BY conname;

-- Functional smoke test: FK still validates immediately for a valid insert.
INSERT INTO defer_parent VALUES (1);
INSERT INTO defer_child VALUES (1, 1);
SELECT id, parent_id FROM defer_child ORDER BY id;

-- ── Reject contradictory NOT DEFERRABLE INITIALLY DEFERRED (all classes) ──────
-- PostgreSQL: "constraint declared INITIALLY DEFERRED must be DEFERRABLE".
-- find-the-class: FK, UNIQUE and PRIMARY KEY must all reject it.

-- FK form
CREATE TABLE defer_bad_fk (
    id INT PRIMARY KEY,
    parent_id INT REFERENCES defer_parent(id) NOT DEFERRABLE INITIALLY DEFERRED
);

-- UNIQUE form
CREATE TABLE defer_bad_uniq (
    id INT PRIMARY KEY,
    code INT,
    CONSTRAINT defer_bad_uniq_u UNIQUE (code) NOT DEFERRABLE INITIALLY DEFERRED
);

-- PRIMARY KEY form
CREATE TABLE defer_bad_pk (
    id INT,
    CONSTRAINT defer_bad_pk_pk PRIMARY KEY (id) NOT DEFERRABLE INITIALLY DEFERRED
);

-- ── DEFERRABLE PRIMARY KEY / UNIQUE round-trip through pg_get_constraintdef ────
CREATE TABLE defer_pk (
    id INT,
    CONSTRAINT defer_pk_pkey PRIMARY KEY (id) DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE defer_uniq_def (
    id INT PRIMARY KEY,
    code INT,
    CONSTRAINT defer_uniq_def_u UNIQUE (code) DEFERRABLE INITIALLY DEFERRED
);

SELECT conname, contype, condeferrable, condeferred, pg_get_constraintdef(oid) AS def
FROM pg_constraint
WHERE conname IN ('defer_pk_pkey', 'defer_uniq_def_u')
ORDER BY conname;

-- ── information_schema.table_constraints reflects deferrability (ORMs read this) ─
-- PG: YES/YES for DEFERRABLE INITIALLY DEFERRED, YES/NO for DEFERRABLE INITIALLY
-- IMMEDIATE, NO/NO for NOT DEFERRABLE — across FK / UNIQUE / PRIMARY KEY.
SELECT constraint_name, constraint_type, is_deferrable, initially_deferred
FROM information_schema.table_constraints
WHERE constraint_name IN (
    'defer_child_fk', 'defer_child_tbl_fk', 'defer_child_nd_parent_id_fkey',
    'defer_uniq_u', 'defer_uniq_def_u', 'defer_pk_pkey'
)
ORDER BY constraint_name;

-- ── pg_index.indimmediate is false for any DEFERRABLE unique/PK index ───────────
-- PG sets indimmediate=false whenever the backing constraint is DEFERRABLE
-- (uniqueness is checked at statement/commit time), independent of
-- INITIALLY IMMEDIATE vs DEFERRED.
SELECT c.relname, i.indimmediate
FROM pg_index i
JOIN pg_class c ON c.oid = i.indexrelid
WHERE c.relname IN ('defer_pk_pkey', 'defer_uniq_def_u', 'defer_uniq_u')
ORDER BY c.relname;

-- ── No phantom information_schema row for a named single-column UNIQUE ──────────
-- `code INT CONSTRAINT named UNIQUE` is backed by the named constraint index.
-- PG surfaces ONLY that constraint — db9 must not also synthesize a default
-- `defer_named_uniq_code_key` row. (#2683 P2)
CREATE TABLE defer_named_uniq (
    id INT PRIMARY KEY,
    code INT CONSTRAINT defer_named_uniq_named UNIQUE
);

SELECT constraint_name, constraint_type, is_deferrable, initially_deferred
FROM information_schema.table_constraints
WHERE table_name = 'defer_named_uniq' AND constraint_type = 'UNIQUE'
ORDER BY constraint_name;

-- ── ddl_export round-trips a multi-column DEFERRABLE UNIQUE as a table
-- ── constraint (identity + deferrability preserved), not a bare unique index. ──
-- db9-specific: _db9_sys_export_ddl() has no PG equivalent. The emitted
-- constraint clause mirrors PG's pg_get_constraintdef
-- (`UNIQUE (a, b) DEFERRABLE INITIALLY DEFERRED`, validated above). (#2683 P1)
CREATE TABLE defer_muc (
    a INT,
    b INT,
    CONSTRAINT defer_muc_u UNIQUE (a, b) DEFERRABLE INITIALLY DEFERRED
);

SELECT
    ddl_sql LIKE '%CONSTRAINT defer_muc_u UNIQUE (a, b) DEFERRABLE INITIALLY DEFERRED%'
        AS unique_constraint_emitted,
    ddl_sql LIKE '%CREATE UNIQUE INDEX%' AS leaks_unique_index
FROM _db9_sys_export_ddl()
WHERE object_type = 'table' AND object_name = 'public.defer_muc';

-- The constraint-backing index must NOT be exported as a standalone object.
SELECT count(*) AS standalone_index_rows
FROM _db9_sys_export_ddl()
WHERE object_type = 'index' AND object_name = 'defer_muc_u';
