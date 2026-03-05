-- Regression test for #1332 / #1440: FK constraints with CASCADE/SET NULL/
-- SET DEFAULT on tables without an explicit PK must be rejected at DDL time.
-- NO ACTION and RESTRICT are safe and must be allowed (PG parity).
--
-- db9-specific: PostgreSQL allows all FK actions on no-PK child tables (uses
-- ctid).  db9 only restricts actions that mutate child rows.

-- Setup: parent table with PK
CREATE TABLE fk_req_pk_parent (id INT PRIMARY KEY);

-- ── Should be REJECTED (unsafe actions on no-PK child) ──────────────────────

-- T1: CREATE TABLE with inline FK ON DELETE CASCADE — rejected
CREATE TABLE fk_req_pk_child (parent_id INT REFERENCES fk_req_pk_parent(id) ON DELETE CASCADE);

-- T2: Table-level FK with ON DELETE SET NULL — rejected
CREATE TABLE fk_req_pk_child2 (
    parent_id INT,
    FOREIGN KEY (parent_id) REFERENCES fk_req_pk_parent(id) ON DELETE SET NULL
);

-- T3: ALTER TABLE ADD FK with ON UPDATE CASCADE — rejected
CREATE TABLE fk_req_pk_nopk (val INT);
ALTER TABLE fk_req_pk_nopk ADD CONSTRAINT fk_nopk FOREIGN KEY (val) REFERENCES fk_req_pk_parent(id) ON UPDATE CASCADE;

-- T4: CREATE TABLE with ON UPDATE SET DEFAULT — rejected
CREATE TABLE fk_req_pk_child3 (parent_id INT REFERENCES fk_req_pk_parent(id) ON UPDATE SET DEFAULT);

-- ── Should SUCCEED (safe actions: NO ACTION / RESTRICT on no-PK child) ──────

-- T5: CREATE TABLE with default NO ACTION (implicit) — allowed
CREATE TABLE fk_req_pk_noaction (parent_id INT REFERENCES fk_req_pk_parent(id));

-- T6: CREATE TABLE with explicit RESTRICT — allowed
CREATE TABLE fk_req_pk_restrict (parent_id INT REFERENCES fk_req_pk_parent(id) ON DELETE RESTRICT ON UPDATE RESTRICT);

-- T7: ALTER TABLE ADD FK with NO ACTION — allowed
ALTER TABLE fk_req_pk_nopk ADD CONSTRAINT fk_nopk_ok FOREIGN KEY (val) REFERENCES fk_req_pk_parent(id) ON DELETE NO ACTION;

-- T8: Self-referencing FK on column without unique constraint — rejected
-- PG 17.7: ERROR "there is no unique constraint matching given keys for
-- referenced table"  (the referenced column must have a PK or UNIQUE constraint)
CREATE TABLE fk_req_pk_tree (id INT, parent_id INT REFERENCES fk_req_pk_tree(id));

-- ── DROP PK guard ───────────────────────────────────────────────────────────

-- T9: DROP PK on table with CASCADE FK — rejected
CREATE TABLE fk_req_pk_dpk_cascade (
    id INT PRIMARY KEY,
    pid INT REFERENCES fk_req_pk_parent(id) ON DELETE CASCADE
);
ALTER TABLE fk_req_pk_dpk_cascade DROP CONSTRAINT fk_req_pk_dpk_cascade_pkey;

-- T10: DROP PK on table with NO ACTION FK (empty table) — allowed
CREATE TABLE fk_req_pk_dpk_noaction (
    id INT PRIMARY KEY,
    pid INT REFERENCES fk_req_pk_parent(id)
);
ALTER TABLE fk_req_pk_dpk_noaction DROP CONSTRAINT fk_req_pk_dpk_noaction_pkey;

-- Cleanup
DROP TABLE IF EXISTS fk_req_pk_dpk_noaction;
DROP TABLE IF EXISTS fk_req_pk_dpk_cascade;
DROP TABLE IF EXISTS fk_req_pk_tree;
DROP TABLE IF EXISTS fk_req_pk_noaction;
DROP TABLE IF EXISTS fk_req_pk_restrict;
DROP TABLE fk_req_pk_nopk;
DROP TABLE IF EXISTS fk_req_pk_child3;
DROP TABLE IF EXISTS fk_req_pk_child2;
DROP TABLE IF EXISTS fk_req_pk_child;
DROP TABLE fk_req_pk_parent;
