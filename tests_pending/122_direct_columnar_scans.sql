-- PostgreSQL compatible tests from direct_columnar_scans
-- 4 tests

-- Test 1: statement (line 1)
-- SET direct_columnar_scans_enabled = true;

-- Test 2: statement (line 4)
CREATE TABLE t145232 (
      k INT PRIMARY KEY,
      a INT NOT NULL,
      b INT NOT NULL,
      c INT NOT NULL,
      v INT NOT NULL DEFAULT 5
);

-- Test 3: statement (line 18)
INSERT INTO t145232 VALUES (2,2,2,2);

-- Test 4: query (line 23)
SELECT * FROM t145232 WHERE k = 2;
