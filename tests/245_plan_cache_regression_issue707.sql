-- Plan-cache regression coverage for issue #707.

-- 1) Cross-session table recreate (DROP + CREATE same name).
DROP TABLE IF EXISTS pc_recreate;
CREATE TABLE pc_recreate(v int);
INSERT INTO pc_recreate VALUES (1);

PREPARE pc_recreate_stmt AS SELECT v FROM pc_recreate ORDER BY 1;
EXECUTE pc_recreate_stmt;
EXECUTE pc_recreate_stmt;
EXECUTE pc_recreate_stmt;
EXECUTE pc_recreate_stmt;
EXECUTE pc_recreate_stmt;
DEALLOCATE pc_recreate_stmt;
PREPARE pc_recreate_stmt AS SELECT v FROM pc_recreate ORDER BY 1;
\! psql -X -q -h ${DB9_TEST_HOST:-127.0.0.1} -p ${DB9_TEST_PORT:-5433} -U ${DB9_TEST_USER:-admin} -d ${DB9_TEST_DB:-postgres} -c "DROP TABLE public.pc_recreate; CREATE TABLE public.pc_recreate(v int); INSERT INTO public.pc_recreate VALUES (2);" >/dev/null
DEALLOCATE pc_recreate_stmt;
PREPARE pc_recreate_stmt AS SELECT v FROM pc_recreate ORDER BY 1;
EXECUTE pc_recreate_stmt;
DEALLOCATE pc_recreate_stmt;
DROP TABLE pc_recreate;

-- 2) Index DDL invalidation (DROP INDEX and CREATE INDEX).
DROP TABLE IF EXISTS pc_idx;
CREATE TABLE pc_idx(id int PRIMARY KEY, v int);
INSERT INTO pc_idx SELECT i, i FROM generate_series(1, 2000) AS s(i);
CREATE INDEX pc_idx_v_idx ON pc_idx(v);

PREPARE pc_idx_stmt(int) AS SELECT id FROM pc_idx WHERE v = $1;
EXECUTE pc_idx_stmt(1500);
EXECUTE pc_idx_stmt(1500);
EXECUTE pc_idx_stmt(1500);
EXECUTE pc_idx_stmt(1500);
EXECUTE pc_idx_stmt(1500);
\! psql -X -q -h ${DB9_TEST_HOST:-127.0.0.1} -p ${DB9_TEST_PORT:-5433} -U ${DB9_TEST_USER:-admin} -d ${DB9_TEST_DB:-postgres} -c "DROP INDEX public.pc_idx_v_idx;" >/dev/null
EXECUTE pc_idx_stmt(1500);
\! psql -X -q -h ${DB9_TEST_HOST:-127.0.0.1} -p ${DB9_TEST_PORT:-5433} -U ${DB9_TEST_USER:-admin} -d ${DB9_TEST_DB:-postgres} -c "CREATE INDEX pc_idx_v_idx ON public.pc_idx(v);" >/dev/null
EXECUTE pc_idx_stmt(1500);
DEALLOCATE pc_idx_stmt;
DROP TABLE pc_idx;

-- 3) SQL EXECUTE parity with direct statement path.
DROP TABLE IF EXISTS pc_parity;
CREATE TABLE pc_parity(id int PRIMARY KEY, v int);
INSERT INTO pc_parity VALUES (1, 10), (2, 20), (3, 30);

PREPARE pc_parity_stmt(int) AS SELECT id FROM pc_parity WHERE v > $1 ORDER BY id;
EXECUTE pc_parity_stmt(15);
SELECT id FROM pc_parity WHERE v > 15 ORDER BY id;
DEALLOCATE pc_parity_stmt;
DROP TABLE pc_parity;

-- 4) Transaction rollback cache determinism.
DROP TABLE IF EXISTS pc_txn;
CREATE TABLE pc_txn(id int PRIMARY KEY, v int);
INSERT INTO pc_txn VALUES (1, 100);

PREPARE pc_txn_stmt AS SELECT count(*) FROM pc_txn;
EXECUTE pc_txn_stmt;
EXECUTE pc_txn_stmt;
EXECUTE pc_txn_stmt;
EXECUTE pc_txn_stmt;
EXECUTE pc_txn_stmt;
BEGIN;
CREATE INDEX pc_txn_v_idx ON pc_txn(v);
ROLLBACK;
EXECUTE pc_txn_stmt;
DEALLOCATE pc_txn_stmt;
DROP TABLE pc_txn;
