-- Regression test: ON CONFLICT (col) WHERE predicate DO NOTHING / DO UPDATE.
-- Issue #2228: parser rejected WHERE between conflict target and DO.

-- Setup: table with a partial unique index.
CREATE TABLE oc_where_test (id SERIAL PRIMARY KEY, val TEXT, status TEXT);
CREATE UNIQUE INDEX oc_where_val_active ON oc_where_test (val) WHERE status = 'active';

-- Case 1: Insert first row, then conflict with DO NOTHING.
INSERT INTO oc_where_test (val, status) VALUES ('a', 'active');
INSERT INTO oc_where_test (val, status) VALUES ('a', 'active') ON CONFLICT (val) WHERE status = 'active' DO NOTHING;
SELECT val, status FROM oc_where_test ORDER BY id;

-- Case 2: Row not matching partial index predicate — no conflict.
INSERT INTO oc_where_test (val, status) VALUES ('a', 'inactive');
SELECT val, status FROM oc_where_test ORDER BY id;

-- Case 3: DO UPDATE with partial index predicate.
INSERT INTO oc_where_test (val, status) VALUES ('a', 'active') ON CONFLICT (val) WHERE status = 'active' DO UPDATE SET status = 'updated';
SELECT val, status FROM oc_where_test ORDER BY id;

-- Case 4: ON CONFLICT (col) DO NOTHING without WHERE (basic, should still work).
CREATE TABLE oc_basic (id INT PRIMARY KEY, val TEXT);
INSERT INTO oc_basic VALUES (1, 'first');
INSERT INTO oc_basic VALUES (1, 'dupe') ON CONFLICT (id) DO NOTHING;
SELECT * FROM oc_basic ORDER BY id;

DROP TABLE oc_basic;
DROP TABLE oc_where_test;

-- Case 5 (P0-1): ON CONFLICT on PK with spurious WHERE — PG allows, ignores WHERE.
CREATE TABLE oc_pk_where (id INT PRIMARY KEY, val TEXT);
INSERT INTO oc_pk_where VALUES (1, 'a');
INSERT INTO oc_pk_where VALUES (1, 'b') ON CONFLICT (id) WHERE id > 0 DO NOTHING;
SELECT * FROM oc_pk_where ORDER BY id;
DROP TABLE oc_pk_where;

-- Case 6 (P0-2): ON CONFLICT (col) DO NOTHING without WHERE, only partial index exists.
-- PG errors: "there is no unique or exclusion constraint matching the ON CONFLICT specification"
CREATE TABLE oc_partial_only (id SERIAL PRIMARY KEY, val TEXT);
CREATE UNIQUE INDEX oc_partial_only_idx ON oc_partial_only (val) WHERE val IS NOT NULL;
INSERT INTO oc_partial_only (val) VALUES ('a');
INSERT INTO oc_partial_only (val) VALUES ('a') ON CONFLICT (val) DO NOTHING;

DROP TABLE oc_partial_only;

-- Case 7 (P1): Multiple partial indexes on same columns, different predicates.
CREATE TABLE oc_multi (id SERIAL PRIMARY KEY, val TEXT, status TEXT);
CREATE UNIQUE INDEX oc_multi_active ON oc_multi (val) WHERE status = 'active';
CREATE UNIQUE INDEX oc_multi_pending ON oc_multi (val) WHERE status = 'pending';
INSERT INTO oc_multi (val, status) VALUES ('x', 'active');
INSERT INTO oc_multi (val, status) VALUES ('x', 'pending');
-- Conflict on active index only — active row should be skipped.
INSERT INTO oc_multi (val, status) VALUES ('x', 'active') ON CONFLICT (val) WHERE status = 'active' DO NOTHING;
-- Conflict on pending index only — pending row should be updated.
INSERT INTO oc_multi (val, status) VALUES ('x', 'pending') ON CONFLICT (val) WHERE status = 'pending' DO UPDATE SET status = 'done';
SELECT val, status FROM oc_multi ORDER BY id;
DROP TABLE oc_multi;
