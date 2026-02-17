-- Regression test: ALTER TABLE statistics invalidation contract.
--
-- Verifies that:
--   (a) No-op ALTER TABLE operations preserve ANALYZE statistics.
--   (b) Structural ALTER TABLE operations clear statistics.
--
-- Observable via EXPLAIN row estimates:
--   rows=5  → real statistics from ANALYZE (5 rows inserted)
--   rows=1000 → heuristic default (no statistics)

SET client_min_messages = warning;
DROP TABLE IF EXISTS t225_stats;

CREATE TABLE t225_stats (id INT PRIMARY KEY, name TEXT, extra INT);
INSERT INTO t225_stats VALUES (1, 'a', 10), (2, 'b', 20), (3, 'c', 30), (4, 'd', 40), (5, 'e', 50);

SET tipg.use_optimizer = on;

-- Phase 1: ANALYZE populates stats (row_count = 5)
ANALYZE t225_stats;

-- E1: EXPLAIN should show rows=5 (from real statistics)
EXPLAIN SELECT * FROM t225_stats;

-- Phase 2: No-op DROP COLUMN IF EXISTS on absent column — stats preserved
ALTER TABLE t225_stats DROP COLUMN IF EXISTS nonexistent_col;

-- E2: EXPLAIN should still show rows=5
EXPLAIN SELECT * FROM t225_stats;

-- Phase 3: No-op ALTER COLUMN SET DATA TYPE to same type — stats preserved
ALTER TABLE t225_stats ALTER COLUMN name TYPE TEXT;

-- E3: EXPLAIN should still show rows=5
EXPLAIN SELECT * FROM t225_stats;

-- Phase 4: Structural ALTER TABLE ADD COLUMN — stats cleared
ALTER TABLE t225_stats ADD COLUMN new_col INT;

-- E4: EXPLAIN should show rows=1000 (heuristic default, stats gone)
EXPLAIN SELECT * FROM t225_stats;

-- Phase 5: Re-ANALYZE restores stats (now 5 rows again)
ANALYZE t225_stats;

-- E5: EXPLAIN should show rows=5 again
EXPLAIN SELECT * FROM t225_stats;

DROP TABLE t225_stats;
