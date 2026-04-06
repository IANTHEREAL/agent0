-- DDL Journal smoke test (#2348)
-- Purpose: Exercise the two DDL paths protected by the crash-recovery journal:
--   1. CREATE INDEX (non-concurrent) with backfill
--   2. CREATE TABLE AS SELECT (CTAS)
-- The journal write/delete wraps these operations transparently; this test
-- confirms normal-path correctness is preserved.

-- ── Setup ──
DROP TABLE IF EXISTS djt_ctas_result;
DROP TABLE IF EXISTS djt_source;

CREATE TABLE djt_source (
    id   SERIAL PRIMARY KEY,
    val  TEXT,
    num  INT
);

INSERT INTO djt_source (val, num)
SELECT 'row_' || g, g
FROM generate_series(1, 500) AS g;

-- ── 1. Non-concurrent CREATE INDEX (journal-protected) ──
CREATE INDEX djt_idx_val ON djt_source (val);
CREATE INDEX djt_idx_num ON djt_source (num);

-- Verify indexes are usable
SELECT count(*) FROM djt_source WHERE val = 'row_250';
SELECT count(*) FROM djt_source WHERE num BETWEEN 100 AND 200;

-- ── 2. CREATE TABLE AS SELECT (journal-protected) ──
CREATE TABLE djt_ctas_result AS
    SELECT id, val, num * 2 AS doubled
    FROM djt_source
    WHERE num <= 100;

SELECT count(*) FROM djt_ctas_result;
SELECT min(doubled), max(doubled) FROM djt_ctas_result;

-- ── 3. Index on CTAS result (also journal-protected) ──
CREATE INDEX djt_ctas_idx ON djt_ctas_result (doubled);
SELECT count(*) FROM djt_ctas_result WHERE doubled = 50;

-- ── Cleanup ──
DROP TABLE djt_ctas_result;
DROP TABLE djt_source;
