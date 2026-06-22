-- DB9 cop pushdown: ordered index TopN only pushes covered ASC/DESC LIMIT+OFFSET bounds.

DROP TABLE IF EXISTS db9_cop_ordered_topn;
DROP TABLE IF EXISTS db9_cop_ordered_topn_desc_on;
DROP TABLE IF EXISTS db9_cop_ordered_topn_desc_off;
DROP TABLE IF EXISTS db9_cop_ordered_topn_mixed_on;
DROP TABLE IF EXISTS db9_cop_ordered_topn_mixed_off;
DROP TABLE IF EXISTS db9_cop_ordered_topn_asc_on;
DROP TABLE IF EXISTS db9_cop_ordered_topn_asc_off;
DROP TABLE IF EXISTS db9_cop_ordered_topn_desc_cover_on;
DROP TABLE IF EXISTS db9_cop_ordered_topn_desc_cover_off;
\! rm -f /tmp/584_pushdown_ordered_topn_desc_explain.txt /tmp/584_pushdown_ordered_topn_mixed_explain.txt /tmp/584_pushdown_ordered_topn_asc_explain.txt /tmp/584_pushdown_ordered_topn_desc_cover_explain.txt

CREATE TABLE db9_cop_ordered_topn(
    id INT PRIMARY KEY,
    tenant_id INT NOT NULL,
    status TEXT NOT NULL,
    created_at INT NOT NULL,
    amount INT NOT NULL
);
CREATE INDEX db9_cop_ordered_topn_idx
    ON db9_cop_ordered_topn(tenant_id, status, created_at, id);

INSERT INTO db9_cop_ordered_topn VALUES
    (1, 1, 'active', 100, 10),
    (2, 1, 'active', 200, 20),
    (3, 1, 'active', 300, 30),
    (4, 1, 'active', 300, 40),
    (5, 1, 'paused', 500, 50),
    (6, 2, 'active', 600, 60);

SET db9.enable_cop_pushdown = on;
\o /tmp/584_pushdown_ordered_topn_desc_explain.txt
EXPLAIN
SELECT id, created_at, amount
FROM db9_cop_ordered_topn
WHERE tenant_id = 1 AND status = 'active'
ORDER BY created_at DESC, id DESC
LIMIT 2 OFFSET 1;
\o
\! if grep -Fq "Sort  (cost=" /tmp/584_pushdown_ordered_topn_desc_explain.txt && grep -Fq "Sort Key: created_at DESC, id DESC" /tmp/584_pushdown_ordered_topn_desc_explain.txt && ! grep -Fq "DB9 Cop Limit:" /tmp/584_pushdown_ordered_topn_desc_explain.txt && ! grep -Fq "DB9 Cop Access:" /tmp/584_pushdown_ordered_topn_desc_explain.txt; then echo "ordered_topn_desc_non_covering_stays_local|1"; else echo "ordered_topn_desc_non_covering_stays_local|0"; fi

SELECT id, created_at, amount
FROM db9_cop_ordered_topn
WHERE tenant_id = 1 AND status = 'active'
ORDER BY created_at DESC, id DESC
LIMIT 2 OFFSET 1;

\o /tmp/584_pushdown_ordered_topn_mixed_explain.txt
EXPLAIN
SELECT id, created_at, amount
FROM db9_cop_ordered_topn
WHERE tenant_id = 1 AND status = 'active'
ORDER BY created_at DESC, id ASC
LIMIT 2 OFFSET 1;
\o
\! if grep -Fq "Sort  (cost=" /tmp/584_pushdown_ordered_topn_mixed_explain.txt && grep -Fq "Sort Key: created_at DESC, id ASC" /tmp/584_pushdown_ordered_topn_mixed_explain.txt && ! grep -Fq "DB9 Cop Limit:" /tmp/584_pushdown_ordered_topn_mixed_explain.txt && ! grep -Fq "DB9 Cop Access:" /tmp/584_pushdown_ordered_topn_mixed_explain.txt; then echo "ordered_topn_mixed_non_covering_stays_local|1"; else echo "ordered_topn_mixed_non_covering_stays_local|0"; fi

SELECT id, created_at, amount
FROM db9_cop_ordered_topn
WHERE tenant_id = 1 AND status = 'active'
ORDER BY created_at DESC, id ASC
LIMIT 2 OFFSET 1;

\o /tmp/584_pushdown_ordered_topn_asc_explain.txt
EXPLAIN
SELECT id, created_at
FROM db9_cop_ordered_topn
WHERE tenant_id = 1 AND status = 'active'
ORDER BY created_at ASC, id ASC
LIMIT 2 OFFSET 1;
\o
\! if grep -Fq "DB9 Cop Access: prefix (1, 'active'), index only" /tmp/584_pushdown_ordered_topn_asc_explain.txt && grep -Fq "DB9 Cop Output: id, created_at" /tmp/584_pushdown_ordered_topn_asc_explain.txt && grep -Fq "DB9 Cop Limit: 3" /tmp/584_pushdown_ordered_topn_asc_explain.txt && ! grep -Fq "Sort  (cost=" /tmp/584_pushdown_ordered_topn_asc_explain.txt && ! grep -Fq "Sort Key:" /tmp/584_pushdown_ordered_topn_asc_explain.txt; then echo "ordered_topn_asc_avoids_local_sort|1"; else echo "ordered_topn_asc_avoids_local_sort|0"; fi

SELECT id, created_at
FROM db9_cop_ordered_topn
WHERE tenant_id = 1 AND status = 'active'
ORDER BY created_at ASC, id ASC
LIMIT 2 OFFSET 1;

\o /tmp/584_pushdown_ordered_topn_desc_cover_explain.txt
EXPLAIN
SELECT id, created_at
FROM db9_cop_ordered_topn
WHERE tenant_id = 1 AND status = 'active'
ORDER BY created_at DESC, id DESC
LIMIT 2 OFFSET 1;
\o
\! if grep -Fq "DB9 Cop Access: prefix (1, 'active'), scan desc, index only" /tmp/584_pushdown_ordered_topn_desc_cover_explain.txt && grep -Fq "DB9 Cop Output: id, created_at" /tmp/584_pushdown_ordered_topn_desc_cover_explain.txt && grep -Fq "DB9 Cop Limit: 3" /tmp/584_pushdown_ordered_topn_desc_cover_explain.txt && ! grep -Fq "Sort  (cost=" /tmp/584_pushdown_ordered_topn_desc_cover_explain.txt && ! grep -Fq "Sort Key:" /tmp/584_pushdown_ordered_topn_desc_cover_explain.txt; then echo "ordered_topn_desc_cover_avoids_local_sort|1"; else echo "ordered_topn_desc_cover_avoids_local_sort|0"; fi

SELECT id, created_at
FROM db9_cop_ordered_topn
WHERE tenant_id = 1 AND status = 'active'
ORDER BY created_at DESC, id DESC
LIMIT 2 OFFSET 1;

CREATE TEMP TABLE db9_cop_ordered_topn_desc_on AS
SELECT id, created_at, amount
FROM db9_cop_ordered_topn
WHERE tenant_id = 1 AND status = 'active'
ORDER BY created_at DESC, id DESC
LIMIT 2 OFFSET 1;

CREATE TEMP TABLE db9_cop_ordered_topn_mixed_on AS
SELECT id, created_at, amount
FROM db9_cop_ordered_topn
WHERE tenant_id = 1 AND status = 'active'
ORDER BY created_at DESC, id ASC
LIMIT 2 OFFSET 1;

CREATE TEMP TABLE db9_cop_ordered_topn_asc_on AS
SELECT id, created_at
FROM db9_cop_ordered_topn
WHERE tenant_id = 1 AND status = 'active'
ORDER BY created_at ASC, id ASC
LIMIT 2 OFFSET 1;

CREATE TEMP TABLE db9_cop_ordered_topn_desc_cover_on AS
SELECT id, created_at
FROM db9_cop_ordered_topn
WHERE tenant_id = 1 AND status = 'active'
ORDER BY created_at DESC, id DESC
LIMIT 2 OFFSET 1;

SET db9.enable_cop_pushdown = off;
CREATE TEMP TABLE db9_cop_ordered_topn_desc_off AS
SELECT id, created_at, amount
FROM db9_cop_ordered_topn
WHERE tenant_id = 1 AND status = 'active'
ORDER BY created_at DESC, id DESC
LIMIT 2 OFFSET 1;

CREATE TEMP TABLE db9_cop_ordered_topn_mixed_off AS
SELECT id, created_at, amount
FROM db9_cop_ordered_topn
WHERE tenant_id = 1 AND status = 'active'
ORDER BY created_at DESC, id ASC
LIMIT 2 OFFSET 1;

CREATE TEMP TABLE db9_cop_ordered_topn_asc_off AS
SELECT id, created_at
FROM db9_cop_ordered_topn
WHERE tenant_id = 1 AND status = 'active'
ORDER BY created_at ASC, id ASC
LIMIT 2 OFFSET 1;

CREATE TEMP TABLE db9_cop_ordered_topn_desc_cover_off AS
SELECT id, created_at
FROM db9_cop_ordered_topn
WHERE tenant_id = 1 AND status = 'active'
ORDER BY created_at DESC, id DESC
LIMIT 2 OFFSET 1;

SELECT 'ordered_topn_desc_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_ordered_topn_desc_on
            EXCEPT ALL
            SELECT * FROM db9_cop_ordered_topn_desc_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_ordered_topn_desc_off
            EXCEPT ALL
            SELECT * FROM db9_cop_ordered_topn_desc_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

SELECT 'ordered_topn_mixed_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_ordered_topn_mixed_on
            EXCEPT ALL
            SELECT * FROM db9_cop_ordered_topn_mixed_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_ordered_topn_mixed_off
            EXCEPT ALL
            SELECT * FROM db9_cop_ordered_topn_mixed_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

SELECT 'ordered_topn_asc_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_ordered_topn_asc_on
            EXCEPT ALL
            SELECT * FROM db9_cop_ordered_topn_asc_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_ordered_topn_asc_off
            EXCEPT ALL
            SELECT * FROM db9_cop_ordered_topn_asc_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

SELECT 'ordered_topn_desc_cover_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_ordered_topn_desc_cover_on
            EXCEPT ALL
            SELECT * FROM db9_cop_ordered_topn_desc_cover_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_ordered_topn_desc_cover_off
            EXCEPT ALL
            SELECT * FROM db9_cop_ordered_topn_desc_cover_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

DROP TABLE db9_cop_ordered_topn;
DROP TABLE db9_cop_ordered_topn_desc_on;
DROP TABLE db9_cop_ordered_topn_desc_off;
DROP TABLE db9_cop_ordered_topn_mixed_on;
DROP TABLE db9_cop_ordered_topn_mixed_off;
DROP TABLE db9_cop_ordered_topn_asc_on;
DROP TABLE db9_cop_ordered_topn_asc_off;
DROP TABLE db9_cop_ordered_topn_desc_cover_on;
DROP TABLE db9_cop_ordered_topn_desc_cover_off;
\! rm -f /tmp/584_pushdown_ordered_topn_desc_explain.txt /tmp/584_pushdown_ordered_topn_mixed_explain.txt /tmp/584_pushdown_ordered_topn_asc_explain.txt /tmp/584_pushdown_ordered_topn_desc_cover_explain.txt
