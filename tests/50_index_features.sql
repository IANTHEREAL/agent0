-- Advanced index metadata (partial / expression / USING gin/gist)

DROP TABLE IF EXISTS ix_t;
CREATE TABLE ix_t (id INT PRIMARY KEY, a INT, b TEXT);

CREATE INDEX ix_t_a_partial ON ix_t (a) WHERE a IS NOT NULL;
CREATE INDEX ix_t_expr_lower ON ix_t ((lower(b)));
CREATE INDEX ix_t_gin ON ix_t USING gin (b);
CREATE INDEX ix_t_gist ON ix_t USING gist (a);

SELECT 'IDXDEF=' || indexname || ':' || lower(indexdef)
FROM pg_catalog.pg_indexes
WHERE schemaname = 'public' AND tablename = 'ix_t'
ORDER BY indexname;

SELECT 'AM=' || oid || ':' || amname FROM pg_catalog.pg_am ORDER BY oid;

SELECT 'PG_GET_INDEXDEF=' || lower(pg_get_indexdef(indexrelid))
FROM pg_catalog.pg_index
WHERE indexdef ILIKE '%ix_t_a_partial%'
ORDER BY indexrelid
LIMIT 1;

-- Ensure standalone calls work (no pg_index row context)
SELECT 'PG_GET_INDEXDEF_STANDALONE='
    || lower(
        pg_get_indexdef(
            (
                SELECT indexrelid
                FROM pg_catalog.pg_index
                WHERE indexdef ILIKE '%ix_t_a_partial%'
                ORDER BY indexrelid
                LIMIT 1
            )
        )
    );

DROP TABLE ix_t;
