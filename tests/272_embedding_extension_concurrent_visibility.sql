-- DB9_DIVERGENCE(#1421): explicit-transaction extension visibility uses one
-- transaction-consistent source for embedding gate checks.
-- db9-specific: `embedding` extension is built-in in db9-server and not available in vanilla PostgreSQL.
-- Contract covered here:
-- 1) txn starts with extension installed, concurrent DROP happens -> call stays visible in txn.
-- 2) txn starts without extension, concurrent CREATE happens -> call stays not visible in txn.
-- This file intentionally includes one expected SQL error; see companion
-- `272_embedding_extension_concurrent_visibility.errors`.

DROP EXTENSION IF EXISTS embedding;
CREATE EXTENSION embedding;

BEGIN;
\! psql -X -q -h ${DB9_TEST_HOST:-127.0.0.1} -p ${DB9_TEST_PORT:-5433} -U ${DB9_TEST_USER:-admin} -d ${DB9_TEST_DB:-postgres} -v ON_ERROR_STOP=1 -c "DROP EXTENSION IF EXISTS embedding;" >/dev/null 2>&1
SELECT
    CASE
        WHEN (SELECT COUNT(*) FROM extensions.embedding_usage()) = 1 THEN 'drop_not_visible_ok'
        ELSE 'drop_not_visible_bad'
    END AS concurrent_drop_visibility;
ROLLBACK;

DROP EXTENSION IF EXISTS embedding;

\set ON_ERROR_STOP off
BEGIN;
\! psql -X -q -h ${DB9_TEST_HOST:-127.0.0.1} -p ${DB9_TEST_PORT:-5433} -U ${DB9_TEST_USER:-admin} -d ${DB9_TEST_DB:-postgres} -v ON_ERROR_STOP=1 -c "CREATE EXTENSION IF NOT EXISTS embedding;" >/dev/null 2>&1
SELECT * FROM extensions.embedding_usage();
ROLLBACK;
\set ON_ERROR_STOP on

DROP EXTENSION IF EXISTS embedding;
