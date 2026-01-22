-- PostgreSQL compatible tests from optimizer_timeout
-- 4 tests

-- Test 1: statement (line 2)
CREATE TABLE table1 (
    col1_0 INT NULL,
    col1_1 BYTEA[] NOT NULL,
    col1_2 INT NULL,
    col1_3 INT NOT NULL,
    col1_4 INT NOT NULL,
    col1_5 FLOAT8,
    col1_6 TIMETZ NOT NULL,
    col1_7 UUID,
    col1_8 INT NOT NULL,
    col2 INT GENERATED ALWAYS AS (col1_0 + 1) STORED,
    col3 INT GENERATED ALWAYS AS (col1_2 + 1) STORED,
    PRIMARY KEY (col1_3, col1_6)
);

-- CockroachDB table/index syntax adaptations.
CREATE INDEX table1_col1_5_col1_1_idx ON table1 (col1_5 ASC, col1_1 ASC);
CREATE UNIQUE INDEX table1_col1_4_col1_3_col1_7_col1_8_col1_6_uidx
    ON table1 (col1_4 DESC, col1_3 ASC, col1_7, col1_8 ASC, col1_6)
    INCLUDE (col1_1);
CREATE INDEX table1_col1_0_col1_2_idx
    ON table1 (col1_0 ASC, col1_2 ASC)
    WHERE ((((col1_6 = '24:00:00-15:59:00'::TIMETZ) AND (col1_5 <= '-Inf'::FLOAT8)) OR (col1_4 < 0)) AND (col1_3 >= 0));
CREATE INDEX table1_col1_2_col1_8_col1_0_expr_col1_4_idx
    ON table1 (col1_2 DESC, col1_8, col1_0 DESC, col2 ASC, col1_4 DESC)
    INCLUDE (col1_1, col1_5, col1_7)
    WHERE col1_3 = 1;
CREATE INDEX table1_expr_idx
    ON table1 (col3 DESC)
    INCLUDE (col1_1, col1_2, col1_4, col1_5, col1_7)
    WHERE col1_5 < '+Inf'::FLOAT8;

-- Test 2: statement (line 27)
SET statement_timeout='0.1s';

-- Test 3: statement (line 30)
SELECT
  tab_124176.col1_4 AS col_298240, tab_124184.col1_8 AS col_298241
FROM
  table1 AS tab_124176,
  table1 AS tab_124177
  JOIN table1 AS tab_124178
    JOIN table1 AS tab_124179
      JOIN table1 AS tab_124180
        JOIN table1 AS tab_124181 ON
            (tab_124180.col1_0) = (tab_124181.col1_0)
            AND (tab_124180.col1_6) = (tab_124181.col1_6)
            AND (tab_124180.col1_3) = (tab_124181.col1_3)
            AND (tab_124180.col1_2) = (tab_124181.col1_2)
        JOIN table1 AS tab_124182 ON (tab_124181.col1_2) = (tab_124182.col1_8) AND (tab_124180.col1_2)::OID = (tab_124182.tableoid) ON
          (tab_124179.col1_2) = (tab_124180.col1_2)
      JOIN table1 AS tab_124183 ON (tab_124182.col3) = (tab_124183.col1_4) AND (tab_124182.col1_3) = (tab_124183.col3)
      JOIN table1 AS tab_124184
        JOIN table1 AS tab_124185 ON
            (tab_124184.col1_2) = (tab_124185.col1_2)
            AND (tab_124184.col1_6) = (tab_124185.col1_6)
            AND (tab_124184.col1_3) = (tab_124185.col1_3)
            AND (tab_124184.col1_4) = (tab_124185.col1_4) ON (tab_124183.col1_8) = (tab_124184.col1_8) ON
        (tab_124178.col1_2) = (tab_124180.col1_2) AND (tab_124178.col1_2) = (tab_124185.col1_8) AND (tab_124178.col2) = (tab_124182.col3) ON
      (tab_124177.col1_8) = (tab_124181.col1_2) AND (tab_124177.col3) = (tab_124178.col2) AND (tab_124177.col3) = (tab_124183.col1_3);

-- Test 4: statement (line 56)
RESET statement_timeout;
