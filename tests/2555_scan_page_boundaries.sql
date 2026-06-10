-- Page-boundary coverage for streaming scans (#2555). TABLE_SCAN_BATCH_SIZE
-- is 1024 rows, so 2500 rows cross three pages; sums and distinct counts
-- prove no row is duplicated or dropped across continuation keys.

DROP TABLE IF EXISTS t2555_pages;
CREATE TABLE t2555_pages (id INT PRIMARY KEY, grp INT, payload TEXT);
INSERT INTO t2555_pages
SELECT g, g % 7, 'p' || g::TEXT
FROM generate_series(1, 2500) AS g;

-- Full scan across three pages: count/distinct/sum detect dup or missing rows.
SELECT 'full_scan=' || COUNT(*)::TEXT || ',' || COUNT(DISTINCT id)::TEXT ||
       ',' || SUM(id)::TEXT || ',' || MIN(id)::TEXT || ',' || MAX(id)::TEXT AS full_scan
FROM t2555_pages;

-- Streaming scan feeding a grouped hash aggregate across pages.
SELECT 'grouped=' || COUNT(*)::TEXT || ',' || SUM(cnt)::TEXT AS grouped
FROM (SELECT grp, COUNT(*) AS cnt FROM t2555_pages GROUP BY grp) s;

-- LIMIT at zero, exactly one page, one past a page, two pages, and beyond all.
SELECT 'limit0=' || COUNT(*)::TEXT AS limit0
FROM (SELECT id FROM t2555_pages LIMIT 0) s;
SELECT 'limit1024=' || COUNT(*)::TEXT || ',' || SUM(id)::TEXT AS limit1024
FROM (SELECT id FROM t2555_pages ORDER BY id LIMIT 1024) s;
SELECT 'limit1025=' || COUNT(*)::TEXT || ',' || SUM(id)::TEXT AS limit1025
FROM (SELECT id FROM t2555_pages ORDER BY id LIMIT 1025) s;
SELECT 'limit2048=' || COUNT(*)::TEXT AS limit2048
FROM (SELECT id FROM t2555_pages LIMIT 2048) s;
SELECT 'limit_all=' || COUNT(*)::TEXT AS limit_all
FROM (SELECT id FROM t2555_pages LIMIT 99999) s;

-- PK-prefix range scan crossing a page boundary (1500 rows under one prefix).
DROP TABLE IF EXISTS t2555_prefix;
CREATE TABLE t2555_prefix (a INT, b INT, v TEXT, PRIMARY KEY (a, b));
INSERT INTO t2555_prefix SELECT 1, g, 'x' FROM generate_series(1, 1500) AS g;
INSERT INTO t2555_prefix SELECT 2, g, 'y' FROM generate_series(1, 10) AS g;

SELECT 'prefix1=' || COUNT(*)::TEXT || ',' || COUNT(DISTINCT b)::TEXT ||
       ',' || SUM(b)::TEXT AS prefix1
FROM t2555_prefix WHERE a = 1;
SELECT 'prefix2=' || COUNT(*)::TEXT || ',' || SUM(b)::TEXT AS prefix2
FROM t2555_prefix WHERE a = 2;
SELECT 'prefix_exact_page=' || COUNT(*)::TEXT AS prefix_exact_page
FROM (SELECT b FROM t2555_prefix WHERE a = 1 ORDER BY b LIMIT 1024) s;

DROP TABLE t2555_prefix;
DROP TABLE t2555_pages;
