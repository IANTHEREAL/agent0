-- Window function PostgreSQL parity tests (issue #595)
-- Tests: RANGE/GROUPS frame modes, new functions, Numeric formatting, FILTER, named WINDOW

DROP TABLE IF EXISTS win_parity;

CREATE TABLE win_parity(dept TEXT, name TEXT, salary INT);

INSERT INTO win_parity VALUES
  ('A', 'Alice',   100),
  ('A', 'Bob',     100),
  ('A', 'Charlie', 200),
  ('B', 'Dave',    150),
  ('B', 'Eve',     300),
  ('B', 'Frank',   300);

-- =============================================================
-- P0: RANGE frame mode (default when ORDER BY is present)
-- PostgreSQL default: RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
-- Peers (equal ORDER BY values) must be included together.
-- =============================================================

-- V1: SUM with ORDER BY — default RANGE frame, peers included
SELECT dept, name, salary,
       SUM(salary) OVER (PARTITION BY dept ORDER BY salary) AS running_sum
FROM win_parity ORDER BY dept, salary, name;

-- V2: COUNT with ORDER BY — default RANGE frame
SELECT dept, name, salary,
       COUNT(*) OVER (PARTITION BY dept ORDER BY salary) AS running_count
FROM win_parity ORDER BY dept, salary, name;

-- V3: AVG with ORDER BY — RANGE frame + Numeric formatting
SELECT dept, name, salary,
       AVG(salary) OVER (PARTITION BY dept ORDER BY salary) AS running_avg
FROM win_parity ORDER BY dept, salary, name;

-- =============================================================
-- P1: New window functions
-- =============================================================

-- V4: NTILE(2) — divide partitions into 2 buckets
SELECT dept, name, salary,
       NTILE(2) OVER (PARTITION BY dept ORDER BY salary) AS bucket
FROM win_parity ORDER BY dept, salary, name;

-- V5: NTILE(3) — 3 buckets, tests uneven distribution
SELECT dept, name, salary,
       NTILE(3) OVER (PARTITION BY dept ORDER BY salary) AS bucket
FROM win_parity ORDER BY dept, salary, name;

-- V6: PERCENT_RANK
SELECT dept, name, salary,
       PERCENT_RANK() OVER (PARTITION BY dept ORDER BY salary) AS pct_rank
FROM win_parity ORDER BY dept, salary, name;

-- V7: CUME_DIST
SELECT dept, name, salary,
       CUME_DIST() OVER (PARTITION BY dept ORDER BY salary) AS cume
FROM win_parity ORDER BY dept, salary, name;

-- V8: NTH_VALUE
SELECT dept, name, salary,
       NTH_VALUE(name, 2) OVER (PARTITION BY dept ORDER BY salary
           ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) AS second_name
FROM win_parity ORDER BY dept, salary, name;

-- =============================================================
-- P3: Numeric formatting — AVG must match PostgreSQL scale
-- =============================================================

-- V9: AVG over entire partition (no ORDER BY) — tests formatting
SELECT dept,
       AVG(salary) OVER (PARTITION BY dept) AS avg_salary
FROM win_parity ORDER BY dept, salary;

-- =============================================================
-- Explicit ROWS vs RANGE frame
-- =============================================================

-- V10: Explicit ROWS frame — each row gets different running sum even with ties
SELECT dept, name, salary,
       SUM(salary) OVER (PARTITION BY dept ORDER BY salary
           ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS rows_sum
FROM win_parity ORDER BY dept, salary, name;

-- V11: Explicit RANGE frame — peers get same sum
SELECT dept, name, salary,
       SUM(salary) OVER (PARTITION BY dept ORDER BY salary
           RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS range_sum
FROM win_parity ORDER BY dept, salary, name;

-- =============================================================
-- GROUPS frame mode
-- =============================================================

-- V12: GROUPS frame — count peer groups
SELECT dept, name, salary,
       SUM(salary) OVER (PARTITION BY dept ORDER BY salary
           GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW) AS groups_sum
FROM win_parity ORDER BY dept, salary, name;

-- =============================================================
-- Multiple window functions in one query
-- =============================================================

-- V13: Multiple functions
SELECT dept, name, salary,
       ROW_NUMBER() OVER (PARTITION BY dept ORDER BY salary) AS rn,
       RANK() OVER (PARTITION BY dept ORDER BY salary) AS rnk,
       DENSE_RANK() OVER (PARTITION BY dept ORDER BY salary) AS drnk,
       NTILE(2) OVER (PARTITION BY dept ORDER BY salary) AS bucket,
       PERCENT_RANK() OVER (PARTITION BY dept ORDER BY salary) AS pct_rank,
       CUME_DIST() OVER (PARTITION BY dept ORDER BY salary) AS cume
FROM win_parity ORDER BY dept, salary, name;

DROP TABLE win_parity;
