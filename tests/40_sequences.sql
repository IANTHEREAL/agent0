-- Sequences smoke test.
-- Detailed semantics (including cross-connection currval) are asserted in `40_sequences_load.py`.

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

