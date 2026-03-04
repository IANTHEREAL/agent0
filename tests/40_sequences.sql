-- Sequences smoke test.
-- Detailed semantics (including cross-connection currval) are asserted in `40_sequences_load.py`.

-- setval without prior nextval must NOT make lastval() succeed (must be first in session)
DROP SEQUENCE IF EXISTS seq_lv_noprior;
CREATE SEQUENCE seq_lv_noprior;
SELECT setval('seq_lv_noprior', 42);
SELECT lastval();
DROP SEQUENCE seq_lv_noprior;

DROP TABLE IF EXISTS seq_sql_smoke;
DROP SEQUENCE IF EXISTS seq_sql_s;

CREATE SEQUENCE seq_sql_s START WITH 5 INCREMENT BY 2;
SELECT nextval('seq_sql_s');
SELECT currval('seq_sql_s');
SELECT setval('seq_sql_s', 10);
SELECT setval('seq_sql_s', 20, false);
SELECT nextval('seq_sql_s');
DROP SEQUENCE seq_sql_s;

CREATE TABLE seq_sql_smoke (id SERIAL PRIMARY KEY, v INT);
INSERT INTO seq_sql_smoke(v) VALUES (1),(2);
SELECT nextval('seq_sql_smoke_id_seq');
DROP TABLE seq_sql_smoke;

-- lastval() after nextval returns the same value.
DROP SEQUENCE IF EXISTS seq_lastval_s1;
CREATE SEQUENCE seq_lastval_s1;
SELECT nextval('seq_lastval_s1');
SELECT lastval();

-- lastval() tracks the most recent nextval across sequences.
DROP SEQUENCE IF EXISTS seq_lastval_s2;
CREATE SEQUENCE seq_lastval_s2 START WITH 100;
SELECT nextval('seq_lastval_s2');
SELECT lastval();
SELECT currval('seq_lastval_s1');

-- setval(seq, val, true) updates currval but does NOT change lastval.
SELECT setval('seq_lastval_s1', 50);
SELECT currval('seq_lastval_s1');
SELECT lastval();

-- setval(seq, val, false) updates neither currval nor lastval.
SELECT setval('seq_lastval_s1', 200, false);
SELECT currval('seq_lastval_s1');
SELECT lastval();

-- currval for untouched sequence errors.
DROP SEQUENCE IF EXISTS seq_lastval_s3;
CREATE SEQUENCE seq_lastval_s3;
SELECT currval('seq_lastval_s3');

DROP SEQUENCE seq_lastval_s1;
DROP SEQUENCE seq_lastval_s2;
DROP SEQUENCE seq_lastval_s3;

-- Disowned implicit SERIAL sequences should survive DROP TABLE and preserve their counters.
CREATE TABLE seq_sql_disowned_serial (id SERIAL);
INSERT INTO seq_sql_disowned_serial VALUES (DEFAULT);
ALTER SEQUENCE seq_sql_disowned_serial_id_seq OWNED BY NONE;
DROP TABLE seq_sql_disowned_serial;
SELECT nextval('seq_sql_disowned_serial_id_seq');
DROP SEQUENCE seq_sql_disowned_serial_id_seq;

-- Regression: setval() must NOT update lastval() (PostgreSQL parity).
-- lastval() tracks only the most recent nextval() result.
DROP SEQUENCE IF EXISTS seq_lv_a;
DROP SEQUENCE IF EXISTS seq_lv_b;
CREATE SEQUENCE seq_lv_a;
CREATE SEQUENCE seq_lv_b;
SELECT nextval('seq_lv_a');
SELECT setval('seq_lv_b', 999);
SELECT lastval();
DROP SEQUENCE seq_lv_a;
DROP SEQUENCE seq_lv_b;
