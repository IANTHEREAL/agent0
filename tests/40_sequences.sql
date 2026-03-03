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

-- lastval() must return the most recently nextval()-ed value, not an arbitrary one.
DROP SEQUENCE IF EXISTS seq_lv_s1;
DROP SEQUENCE IF EXISTS seq_lv_s2;
CREATE SEQUENCE seq_lv_s1 START WITH 100;
CREATE SEQUENCE seq_lv_s2 START WITH 200;
SELECT nextval('seq_lv_s1');
SELECT nextval('seq_lv_s2');
SELECT lastval();
-- setval on a DIFFERENT sequence must NOT update lastval
SELECT setval('seq_lv_s1', 999);
SELECT lastval();
-- setval on the SAME sequence as the most recent nextval MUST update lastval (PG 17 semantics)
SELECT setval('seq_lv_s2', 555);
SELECT lastval();
DROP SEQUENCE seq_lv_s1;
DROP SEQUENCE seq_lv_s2;

-- setval(same_seq, val, is_called=false) must NOT update lastval (PG 17 parity)
DROP SEQUENCE IF EXISTS seq_lv_iscalled;
CREATE SEQUENCE seq_lv_iscalled START WITH 1;
SELECT nextval('seq_lv_iscalled');
SELECT setval('seq_lv_iscalled', 10, false);
SELECT lastval();
-- setval(same_seq, val, is_called=true) MUST update lastval
SELECT setval('seq_lv_iscalled', 20, true);
SELECT lastval();
-- setval(same_seq, val) with default is_called=true MUST update lastval
SELECT setval('seq_lv_iscalled', 30);
SELECT lastval();
DROP SEQUENCE seq_lv_iscalled;

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
