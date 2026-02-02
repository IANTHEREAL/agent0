-- Regression tests for issue #39:
-- delete_by_pk() must not leave stale secondary index entries.

-- ------------------------------------------------------------
-- 1) ON CONFLICT rollback should clean up prior index entries.
-- ------------------------------------------------------------
DROP TABLE IF EXISTS t_conflict;

CREATE TABLE t_conflict (
    id INT PRIMARY KEY,
    a INT,
    b INT
);

-- Create the non-conflicting index first so it gets materialized before the
-- conflict is detected on the second unique index.
CREATE UNIQUE INDEX t_conflict_b_uidx ON t_conflict(b);
CREATE UNIQUE INDEX t_conflict_a_uidx ON t_conflict(a);

INSERT INTO t_conflict VALUES (1, 1, 100);

-- Conflicts on (a), should not leave a stale unique index entry on (b).
INSERT INTO t_conflict VALUES (2, 1, 200) ON CONFLICT (a) DO NOTHING;

-- Should succeed (no row with b=200 exists).
INSERT INTO t_conflict VALUES (3, 2, 200);

SELECT id, a, b FROM t_conflict ORDER BY id;

DROP TABLE t_conflict;

-- ------------------------------------------------------------
-- 2) FK CASCADE deletes should clean up child indexes.
-- ------------------------------------------------------------
DROP TABLE IF EXISTS t_child;
DROP TABLE IF EXISTS t_parent;

CREATE TABLE t_parent (
    id INT PRIMARY KEY
);

CREATE TABLE t_child (
    id INT PRIMARY KEY,
    parent_id INT REFERENCES t_parent(id) ON DELETE CASCADE,
    u INT
);

CREATE UNIQUE INDEX t_child_u_uidx ON t_child(u);

INSERT INTO t_parent VALUES (1);
INSERT INTO t_child VALUES (1, 1, 42);

DELETE FROM t_parent WHERE id = 1;

-- Should succeed (child row is gone, so unique index entry must be gone too).
INSERT INTO t_parent VALUES (2);
INSERT INTO t_child VALUES (2, 2, 42);

SELECT id, parent_id, u FROM t_child ORDER BY id;

DROP TABLE t_child;
DROP TABLE t_parent;

