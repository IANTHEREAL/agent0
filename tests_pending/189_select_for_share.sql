-- PostgreSQL compatible tests from select_for_share
-- 41 tests

SET client_min_messages = warning;

CREATE EXTENSION IF NOT EXISTS dblink;
CREATE EXTENSION IF NOT EXISTS pgrowlocks;

-- Basic FOR SHARE locking and upgrade-to-update within the same transaction.
DROP TABLE IF EXISTS t;
CREATE TABLE t(a INT PRIMARY KEY);
INSERT INTO t VALUES (1);

BEGIN;
SELECT * FROM t WHERE a = 1 FOR SHARE;
UPDATE t SET a = 2 WHERE a = 1;
COMMIT;

SELECT * FROM t ORDER BY a;

-- FOR SHARE works under SERIALIZABLE isolation in PostgreSQL.
BEGIN ISOLATION LEVEL SERIALIZABLE;
SELECT * FROM t WHERE a = 2 FOR SHARE;
COMMIT;

-- SKIP LOCKED semantics (requires another concurrent session).
DROP TABLE IF EXISTS t;
CREATE TABLE t(a INT PRIMARY KEY);
INSERT INTO t VALUES (1), (2);

SELECT dblink_connect('locker', 'dbname=' || current_database() || ' user=postgres');
SELECT dblink_exec('locker', 'BEGIN');
SELECT * FROM dblink('locker', 'SELECT a FROM t WHERE a = 1 FOR UPDATE') AS locked(a INT);

SELECT COUNT(*) AS locked_rows FROM pgrowlocks('t');

-- With row 1 locked elsewhere, SKIP LOCKED should return only row 2.
SELECT * FROM t ORDER BY a FOR SHARE SKIP LOCKED;
SELECT * FROM t ORDER BY a FOR UPDATE SKIP LOCKED;

SELECT dblink_exec('locker', 'COMMIT');
SELECT dblink_disconnect('locker');

-- With no locks, SKIP LOCKED returns all rows.
SELECT * FROM t ORDER BY a FOR SHARE SKIP LOCKED;
SELECT * FROM t ORDER BY a FOR UPDATE SKIP LOCKED;

RESET client_min_messages;
