-- LASTVAL / CURRVAL multi-sequence determinism test (#1320, #1334)
-- Covers all 9 PG 17.8 semantics constraints.
DROP SEQUENCE IF EXISTS lastval_s1;
DROP SEQUENCE IF EXISTS lastval_s2;
CREATE SEQUENCE lastval_s1;
CREATE SEQUENCE lastval_s2;

-- ============================================================
-- Constraint 3: lastval() with no prior nextval → error
-- ============================================================
-- setval without prior nextval must NOT define lastval
DROP SEQUENCE IF EXISTS lastval_s0;
CREATE SEQUENCE lastval_s0;
SELECT setval('lastval_s0', 50);
SELECT lastval();
DROP SEQUENCE lastval_s0;

-- ============================================================
-- Constraint 1: nextval('s') writes both sentinel and seq name
-- ============================================================
SELECT nextval('lastval_s1');
SELECT nextval('lastval_s2');
SELECT lastval();

-- Switch back to s1
SELECT nextval('lastval_s1');
SELECT lastval();

-- Same-statement multi-call: all lastval() calls must return the same value
SELECT nextval('lastval_s1'), lastval(), lastval();

-- Interleaved nextval calls then lastval
SELECT nextval('lastval_s2');
SELECT nextval('lastval_s1');
SELECT lastval();

-- ============================================================
-- Constraint 2: setval on DIFFERENT sequence → no lastval/currval update
-- ============================================================
SELECT setval('lastval_s2', 100);
SELECT lastval();
-- currval('lastval_s1') must still be the last nextval result for s1
SELECT currval('lastval_s1');
-- currval('lastval_s2') must reflect the setval (is_called=true updates currval)
SELECT currval('lastval_s2');

-- ============================================================
-- Constraint 7: nextval then setval(same, v, true) → lastval = v
-- Constraint 4: setval(same, v, true) → updates lastval AND currval
-- ============================================================
SELECT nextval('lastval_s1');
SELECT setval('lastval_s1', 99);
SELECT lastval();
SELECT currval('lastval_s1');

-- ============================================================
-- Constraint 8: nextval then setval(same, v, false) → lastval = nextval result
-- Constraint 5: setval(same, v, false) → no lastval, no currval update
-- ============================================================
SELECT nextval('lastval_s1');
SELECT setval('lastval_s1', 10, false);
SELECT lastval();
SELECT currval('lastval_s1');

-- ============================================================
-- Constraint 4 again: setval(same, v, true) DOES update lastval + currval
-- ============================================================
SELECT nextval('lastval_s1');
SELECT setval('lastval_s1', 10, true);
SELECT lastval();
SELECT currval('lastval_s1');

-- ============================================================
-- Constraint 6: setval(same, v) default is_called=true → same as constraint 4
-- ============================================================
SELECT nextval('lastval_s1');
SELECT setval('lastval_s1', 10);
SELECT lastval();
SELECT currval('lastval_s1');

-- ============================================================
-- Single-statement: nextval + setval(same) + lastval (#1334 block 4)
-- ============================================================
SELECT nextval('lastval_s1'), setval('lastval_s1', 200), lastval();

-- ============================================================
-- Constraint 9: Sentinel keys use exact match (no starts_with collision)
-- Create sequences with overlapping name prefixes
-- ============================================================
DROP SEQUENCE IF EXISTS lastval_overlap;
DROP SEQUENCE IF EXISTS lastval_overlap_long;
CREATE SEQUENCE lastval_overlap;
CREATE SEQUENCE lastval_overlap_long;
SELECT nextval('lastval_overlap');
SELECT nextval('lastval_overlap_long');
-- lastval should be from lastval_overlap_long, not confused with lastval_overlap
SELECT lastval();
SELECT currval('lastval_overlap_long');
DROP SEQUENCE lastval_overlap;
DROP SEQUENCE lastval_overlap_long;

-- ============================================================
-- SERIAL default path must also update lastval (#1334 block 1)
-- ============================================================
DROP TABLE IF EXISTS lastval_serial_t;
CREATE TABLE lastval_serial_t (id SERIAL PRIMARY KEY, name TEXT);
INSERT INTO lastval_serial_t (name) VALUES ('foo');
SELECT lastval();
INSERT INTO lastval_serial_t (name) VALUES ('bar');
SELECT lastval();
DROP TABLE lastval_serial_t;

-- ============================================================
-- C10: lastval() with arguments must error (SQLSTATE 42883)
-- ============================================================
SELECT lastval(1);
SELECT lastval('x');
SELECT lastval(1.5);
SELECT lastval(true);
SELECT lastval();
SELECT lastval(2147483648);
SELECT lastval(1e2);

-- Cleanup
DROP SEQUENCE lastval_s1;
DROP SEQUENCE lastval_s2;
