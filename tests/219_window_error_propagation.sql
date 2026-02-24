-- Regression: window operator must propagate eval errors, not swallow them as NULL.
-- Issue: https://github.com/c4pt0r/db9/issues/588

DROP TABLE IF EXISTS win_err_t;

CREATE TABLE win_err_t(a INT, b INT);

INSERT INTO win_err_t VALUES (1, 10), (2, 20), (3, 30);

-- ORDER BY with missing column must error (not silently use NULL sort keys).
SELECT row_number() OVER (ORDER BY missing_col) FROM win_err_t;

-- PARTITION BY with missing column must error.
SELECT row_number() OVER (PARTITION BY missing_col ORDER BY a) FROM win_err_t;

-- RANK with missing ORDER BY column must error.
SELECT rank() OVER (ORDER BY missing_col) FROM win_err_t;

-- DENSE_RANK with missing ORDER BY column must error.
SELECT dense_rank() OVER (ORDER BY missing_col) FROM win_err_t;

-- SUM with missing arg column must error.
SELECT sum(missing_col) OVER () FROM win_err_t;

DROP TABLE win_err_t;
