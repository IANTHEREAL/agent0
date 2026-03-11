-- SERIAL implicit sequence vs same-statement constraint name collision (42P07)
-- PostgreSQL rejects when a CONSTRAINT name equals the implicit sequence name.

-- cleanup
DROP TABLE IF EXISTS t1469_pk CASCADE;
DROP TABLE IF EXISTS t1469_u CASCADE;
DROP TABLE IF EXISTS t1469_big CASCADE;
DROP TABLE IF EXISTS t1469_small CASCADE;
DROP TABLE IF EXISTS t1469_ok CASCADE;

-- 1. PK constraint name collides with implicit sequence name → 42P07
CREATE TABLE t1469_pk(id SERIAL CONSTRAINT t1469_pk_id_seq PRIMARY KEY);

-- 2. UNIQUE constraint name collides with implicit sequence name → 42P07
CREATE TABLE t1469_u(id SERIAL, val INT CONSTRAINT t1469_u_id_seq UNIQUE);

-- 3. BIGSERIAL variant → 42P07
CREATE TABLE t1469_big(id BIGSERIAL CONSTRAINT t1469_big_id_seq PRIMARY KEY);

-- 4. SMALLSERIAL variant → 42P07
CREATE TABLE t1469_small(id SMALLSERIAL CONSTRAINT t1469_small_id_seq PRIMARY KEY);

-- 5. Non-colliding constraint name should succeed
CREATE TABLE t1469_ok(id SERIAL CONSTRAINT t1469_ok_pk PRIMARY KEY);
SELECT 'ok_created' AS result;

-- cleanup
DROP TABLE IF EXISTS t1469_pk CASCADE;
DROP TABLE IF EXISTS t1469_u CASCADE;
DROP TABLE IF EXISTS t1469_big CASCADE;
DROP TABLE IF EXISTS t1469_small CASCADE;
DROP TABLE t1469_ok;
