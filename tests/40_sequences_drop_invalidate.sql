-- #1416: DROP TABLE / DROP SCHEMA CASCADE must invalidate session sequence state.

-- T1: DROP TABLE invalidates lastval for owned SERIAL sequence.
DROP TABLE IF EXISTS sdi_t1;
CREATE TABLE sdi_t1 (id SERIAL PRIMARY KEY, name TEXT);
INSERT INTO sdi_t1 (name) VALUES ('a');
SELECT lastval();
DROP TABLE sdi_t1;
SELECT lastval();

-- T2: DROP TABLE does not affect lastval from a different sequence.
DROP SEQUENCE IF EXISTS sdi_s2;
CREATE SEQUENCE sdi_s2;
DROP TABLE IF EXISTS sdi_t2;
CREATE TABLE sdi_t2 (id SERIAL PRIMARY KEY);
SELECT nextval('sdi_t2_id_seq');
SELECT nextval('sdi_s2');
-- lastval is now from sdi_s2
SELECT lastval();
DROP TABLE sdi_t2;
-- lastval should still be from sdi_s2 (not invalidated)
SELECT lastval();
DROP SEQUENCE sdi_s2;

-- T3: DROP SCHEMA CASCADE invalidates lastval for sequences in the schema.
CREATE SCHEMA IF NOT EXISTS sdi_schema;
CREATE SEQUENCE sdi_schema.s3;
SELECT nextval('sdi_schema.s3');
DROP SCHEMA sdi_schema CASCADE;
SELECT lastval();

-- T4: Explicit DROP SEQUENCE invalidates lastval.
DROP SEQUENCE IF EXISTS sdi_s4;
CREATE SEQUENCE sdi_s4;
SELECT nextval('sdi_s4');
SELECT lastval();
DROP SEQUENCE sdi_s4;
SELECT lastval();
