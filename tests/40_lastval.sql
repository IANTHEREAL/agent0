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

-- ============================================================
-- C11: drop/recreate same-name sequence must not leak stale lastval (#1370)
-- PostgreSQL tracks sequences by OID, so drop+recreate = new identity.
-- ============================================================
DROP SEQUENCE IF EXISTS lastval_droprec;
CREATE SEQUENCE lastval_droprec;
SELECT nextval('lastval_droprec');
SELECT lastval();
DROP SEQUENCE lastval_droprec;
-- After drop, lastval must error (session state cleared)
SELECT lastval();
-- Recreate with different START; lastval must still error until nextval
CREATE SEQUENCE lastval_droprec START 100;
SELECT lastval();
-- Now use the recreated sequence
SELECT nextval('lastval_droprec');
SELECT lastval();
-- currval must also reflect the new sequence
SELECT currval('lastval_droprec');
DROP SEQUENCE lastval_droprec;

-- ============================================================
-- C12: DROP SEQUENCE inside rolled-back transaction must not clear lastval (#1408)
-- PostgreSQL defers session-state cleanup to commit; ROLLBACK restores lastval.
-- ============================================================
DROP SEQUENCE IF EXISTS lastval_rollback;
CREATE SEQUENCE lastval_rollback;
SELECT nextval('lastval_rollback');
SELECT lastval();
BEGIN;
DROP SEQUENCE lastval_rollback;
ROLLBACK;
-- lastval must still return the pre-DROP value
SELECT lastval();
-- sequence must still exist after rollback
SELECT nextval('lastval_rollback');
SELECT lastval();
DROP SEQUENCE lastval_rollback;

-- ============================================================
-- C13: drop+recreate same-name sequence in one txn must not wipe new lastval (#1408)
-- Deferred drop must be keyed by identity, not name.
-- ============================================================
DROP SEQUENCE IF EXISTS lastval_recreate;
CREATE SEQUENCE lastval_recreate;
BEGIN;
SELECT nextval('lastval_recreate');
DROP SEQUENCE lastval_recreate;
CREATE SEQUENCE lastval_recreate;
SELECT nextval('lastval_recreate');
COMMIT;
-- lastval must return 1 (from the new sequence), not be wiped by the old drop
SELECT lastval();
SELECT currval('lastval_recreate');
DROP SEQUENCE lastval_recreate;

-- ============================================================
-- C14: lastval() must error after DROP SEQUENCE within explicit txn (#1408)
-- Deferred drop must block reads immediately, not just at commit.
-- ============================================================
DROP SEQUENCE IF EXISTS lastval_txndrop;
CREATE SEQUENCE lastval_txndrop;
SELECT nextval('lastval_txndrop');
SELECT lastval();
BEGIN;
DROP SEQUENCE lastval_txndrop;
-- lastval must error within the transaction
SELECT lastval();
ROLLBACK;
-- After rollback, lastval must be restored
SELECT lastval();
DROP SEQUENCE lastval_txndrop;

-- C15: ROLLBACK TO SAVEPOINT must restore pending_drops (#1408)
-- DROP SEQUENCE inside a savepoint that is then rolled back must not
-- block lastval/currval.
-- ============================================================
DROP SEQUENCE IF EXISTS lastval_sp;
CREATE SEQUENCE lastval_sp;
SELECT nextval('lastval_sp');
BEGIN;
SELECT nextval('lastval_sp');
SAVEPOINT sp1;
DROP SEQUENCE lastval_sp;
-- Inside savepoint after DROP — lastval must error
SELECT lastval();
ROLLBACK TO sp1;
-- After rollback to savepoint — lastval must be restored
SELECT lastval();
COMMIT;
-- After commit — lastval still valid (no stale pending drop)
SELECT lastval();
DROP SEQUENCE lastval_sp;

-- ============================================================
-- C16: DROP+recreate+rollback must invalidate stale lastval identity (#1408)
-- PG tracks by OID; after rollback the recreated OID is gone.
-- lastval() must error, currval() must return the pre-DROP value.
-- ============================================================
DROP SEQUENCE IF EXISTS lastval_droprec_rb;
CREATE SEQUENCE lastval_droprec_rb;
SELECT nextval('lastval_droprec_rb');
SELECT nextval('lastval_droprec_rb');
BEGIN;
DROP SEQUENCE lastval_droprec_rb;
CREATE SEQUENCE lastval_droprec_rb START 100;
SELECT nextval('lastval_droprec_rb');
ROLLBACK;
-- lastval must error — the recreated identity was rolled back
SELECT lastval();
-- currval must return the pre-DROP value (2)
SELECT currval('lastval_droprec_rb');
DROP SEQUENCE lastval_droprec_rb;

-- ============================================================
-- C17: DROP+recreate inside savepoint + ROLLBACK TO (#1408)
-- Same identity invalidation but via savepoint rollback.
-- ============================================================
DROP SEQUENCE IF EXISTS lastval_sp_droprec;
CREATE SEQUENCE lastval_sp_droprec;
SELECT nextval('lastval_sp_droprec');
BEGIN;
SELECT nextval('lastval_sp_droprec');
SAVEPOINT sp1;
DROP SEQUENCE lastval_sp_droprec;
CREATE SEQUENCE lastval_sp_droprec START 100;
SELECT nextval('lastval_sp_droprec');
ROLLBACK TO sp1;
-- lastval must error after savepoint rollback
SELECT lastval();
-- currval must return the pre-savepoint value (2)
SELECT currval('lastval_sp_droprec');
COMMIT;
-- After commit, lastval still errors (identity was invalidated)
SELECT lastval();
SELECT currval('lastval_sp_droprec');
DROP SEQUENCE lastval_sp_droprec;

-- ============================================================
-- C18: Multi-cycle drop+recreate with savepoint rollback (#1408 R8)
-- Tests that reobserved_drops is not deduplicated by name.
-- Second cycle must be properly undone by ROLLBACK TO s2.
-- ============================================================
DROP SEQUENCE IF EXISTS lastval_mc;
CREATE SEQUENCE lastval_mc;
SELECT nextval('lastval_mc');
BEGIN;
SAVEPOINT s1;
DROP SEQUENCE lastval_mc;
CREATE SEQUENCE lastval_mc START 100;
SELECT nextval('lastval_mc');
SAVEPOINT s2;
DROP SEQUENCE lastval_mc;
CREATE SEQUENCE lastval_mc START 200;
SELECT nextval('lastval_mc');
ROLLBACK TO s2;
-- currval must return first cycle's value (100), not second's (200)
SELECT currval('lastval_mc');
COMMIT;
DROP SEQUENCE lastval_mc;

-- Cleanup
DROP SEQUENCE lastval_s1;
DROP SEQUENCE lastval_s2;
