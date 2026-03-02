-- Vector extension bootstrap compatibility contract (ORM/agent-safe).
-- Covers:
-- 1) CREATE EXTENSION IF NOT EXISTS vector succeeds and is idempotent
-- 2) pg_extension metadata row exists after bootstrap
-- 3) vector usage works after bootstrap
-- 4) bootstrap inside explicit transaction does not poison the transaction

DROP EXTENSION IF EXISTS vector;
DROP TABLE IF EXISTS vec_bootstrap_contract;

CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS vector;

SELECT COUNT(*) AS vector_ext_rows
FROM pg_extension
WHERE extname = 'vector';

CREATE TABLE vec_bootstrap_contract (
  id INT PRIMARY KEY,
  emb vector(3)
);

INSERT INTO vec_bootstrap_contract VALUES
  (1, '[1,0,0]'),
  (2, '[0,1,0]');

SELECT id, emb <=> '[1,0,0]'::vector(3) AS cos_dist
FROM vec_bootstrap_contract
ORDER BY id;

BEGIN;
CREATE EXTENSION IF NOT EXISTS vector;
SELECT 1 AS txn_alive;
ROLLBACK;

DROP TABLE vec_bootstrap_contract;
DROP EXTENSION IF EXISTS vector;
