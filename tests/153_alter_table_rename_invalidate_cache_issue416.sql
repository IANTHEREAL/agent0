-- Issue #416: `list_tables` cache must be invalidated on ALTER TABLE RENAME.
--
-- Repro:
-- 1) Warm `list_tables` cache via an `information_schema` query.
-- 2) Rename a table.
-- 3) Re-query `information_schema.tables` immediately.
-- Expected: only the new name is present (no TTL wait).

DROP TABLE IF EXISTS cache_rename_issue416_t2;
DROP TABLE IF EXISTS cache_rename_issue416_t;

CREATE TABLE cache_rename_issue416_t(id INT);

-- Warm `list_tables` cache.
SELECT table_name
FROM information_schema.tables
WHERE table_schema = 'public'
  AND table_name IN ('cache_rename_issue416_t', 'cache_rename_issue416_t2')
ORDER BY table_name;

ALTER TABLE cache_rename_issue416_t RENAME TO cache_rename_issue416_t2;

-- Must reflect rename immediately (no cache TTL window).
SELECT table_name
FROM information_schema.tables
WHERE table_schema = 'public'
  AND table_name IN ('cache_rename_issue416_t', 'cache_rename_issue416_t2')
ORDER BY table_name;

DROP TABLE cache_rename_issue416_t2;
