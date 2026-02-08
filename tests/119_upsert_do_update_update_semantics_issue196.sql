-- Regression test for issue #196:
-- UPSERT DO UPDATE must preserve UPDATE semantics (FK checks / ON UPDATE actions / PK+index re-key).

-- Case 1: FK checks must run on the *updated row* (not just the excluded/insert row).
DROP TABLE IF EXISTS t_issue196_fk_child;
DROP TABLE IF EXISTS t_issue196_fk_parent;

CREATE TABLE t_issue196_fk_parent (id INT PRIMARY KEY);
CREATE TABLE t_issue196_fk_child (
    id INT PRIMARY KEY,
    parent_id INT REFERENCES t_issue196_fk_parent(id)
);

INSERT INTO t_issue196_fk_parent VALUES (1);
INSERT INTO t_issue196_fk_child VALUES (1, 1);

-- excluded parent_id is valid (1), but DO UPDATE computes invalid (2).
-- Expected: ERROR (FK violation) and table remains unchanged.
INSERT INTO t_issue196_fk_child(id, parent_id)
VALUES (1, 1)
ON CONFLICT (id) DO UPDATE SET parent_id = EXCLUDED.parent_id + 1;

SELECT * FROM t_issue196_fk_child ORDER BY id;

DROP TABLE t_issue196_fk_child;
DROP TABLE t_issue196_fk_parent;

-- Case 2: ON UPDATE CASCADE + PK/index re-key should work for DO UPDATE that changes the PK.
DROP TABLE IF EXISTS t_issue196_cascade_child;
DROP TABLE IF EXISTS t_issue196_cascade_parent;

CREATE TABLE t_issue196_cascade_parent (
    id INT PRIMARY KEY,
    u INT UNIQUE
);

CREATE TABLE t_issue196_cascade_child (
    id INT PRIMARY KEY,
    pid INT REFERENCES t_issue196_cascade_parent(id) ON UPDATE CASCADE
);

INSERT INTO t_issue196_cascade_parent VALUES (1, 10);
INSERT INTO t_issue196_cascade_child VALUES (1, 1);

-- Conflict on UNIQUE(u); DO UPDATE changes PK id.
INSERT INTO t_issue196_cascade_parent(id, u)
VALUES (2, 10)
ON CONFLICT (u) DO UPDATE SET id = EXCLUDED.id;

SELECT * FROM t_issue196_cascade_parent ORDER BY id;
SELECT * FROM t_issue196_cascade_child ORDER BY id;

-- Verify unique predicate lookup returns the updated PK (index re-key correctness).
SELECT id, u FROM t_issue196_cascade_parent WHERE u = 10;

DROP TABLE t_issue196_cascade_child;
DROP TABLE t_issue196_cascade_parent;

-- Case 3: CHECK constraints must be enforced on the *updated row* (UPSERT must not bypass CHECK).
DROP TABLE IF EXISTS t_issue196_check;

CREATE TABLE t_issue196_check (
    id INT PRIMARY KEY,
    i INT CONSTRAINT t_issue196_check_i_positive CHECK (i > 0)
);

INSERT INTO t_issue196_check VALUES (1, 1);

-- Expected: ERROR (CHECK violation) and table remains unchanged.
INSERT INTO t_issue196_check(id, i)
VALUES (1, -1)
ON CONFLICT (id) DO UPDATE SET i = EXCLUDED.i;

SELECT * FROM t_issue196_check ORDER BY id;

DROP TABLE t_issue196_check;

-- Case 4: UPSERT DO UPDATE must execute BEFORE UPDATE triggers (and allow BEFORE INSERT triggers
-- to populate NOT NULL columns on the excluded row).
DROP TRIGGER IF EXISTS t_issue196_bi ON t_issue196_upsert_trg;
DROP TRIGGER IF EXISTS t_issue196_bu ON t_issue196_upsert_trg;
DROP TABLE IF EXISTS t_issue196_upsert_trg;
DROP FUNCTION IF EXISTS t_issue196_bi();
DROP FUNCTION IF EXISTS t_issue196_bu();

CREATE TABLE t_issue196_upsert_trg (
    id INT PRIMARY KEY,
    n INT NOT NULL
);

CREATE OR REPLACE FUNCTION t_issue196_bi()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
  NEW.n := 1;
  RETURN NEW;
END;
$$;

CREATE OR REPLACE FUNCTION t_issue196_bu()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
  NEW.n := OLD.n + 1;
  RETURN NEW;
END;
$$;

CREATE TRIGGER t_issue196_bi BEFORE INSERT ON t_issue196_upsert_trg
FOR EACH ROW EXECUTE PROCEDURE t_issue196_bi();

CREATE TRIGGER t_issue196_bu BEFORE UPDATE ON t_issue196_upsert_trg
FOR EACH ROW EXECUTE PROCEDURE t_issue196_bu();

INSERT INTO t_issue196_upsert_trg(id) VALUES (1);

-- PK conflict: DO UPDATE assigns EXCLUDED.n (set to 1 by BEFORE INSERT),
-- then BEFORE UPDATE increments to 2.
INSERT INTO t_issue196_upsert_trg(id)
VALUES (1)
ON CONFLICT (id) DO UPDATE SET n = EXCLUDED.n;

SELECT * FROM t_issue196_upsert_trg ORDER BY id;

DROP TRIGGER t_issue196_bi ON t_issue196_upsert_trg;
DROP TRIGGER t_issue196_bu ON t_issue196_upsert_trg;
DROP TABLE t_issue196_upsert_trg;
DROP FUNCTION t_issue196_bi();
DROP FUNCTION t_issue196_bu();
