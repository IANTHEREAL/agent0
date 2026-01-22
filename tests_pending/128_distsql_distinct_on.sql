-- PostgreSQL compatible tests from distsql_distinct_on
-- 8 tests

-- Test 1: statement (line 3)
CREATE TABLE xyz (
  id INT PRIMARY KEY,
  x INT,
  y INT,
  z INT
);

-- Test 2: statement (line 11)
INSERT INTO xyz VALUES
  (1, 1, 1, NULL),
  (2, 1, 1, 2),
  (3, 1, 1, 2),
  (4, 1, 2, 1),
  (5, 2, 2, 3),
  (6, 4, 5, 6),
  (7, 4, 1, 6);

-- Test 3: statement (line 29)
CREATE TABLE abc (
  a TEXT,
  b TEXT,
  c TEXT
);

INSERT INTO abc VALUES
  ('1', '1', '1'),
  ('1', '1', '2'),
  ('1', '2', '2'),
  ('2', '3', '4'),
  ('3', '4', '5');

-- Test 4: query (line 88)
SELECT DISTINCT ON (x,y,z) x, y, z FROM xyz;

-- Test 5: query (line 98)
SELECT DISTINCT ON (x,y,z) x, y, z FROM xyz ORDER BY x;

-- Test 6: query (line 108)
SELECT DISTINCT ON (y) x, y FROM xyz ORDER BY y, x;

-- Test 7: query (line 115)
SELECT DISTINCT ON (a,b,c) a, b, c FROM abc;

-- Test 8: query (line 124)
SELECT DISTINCT ON (a, b) a, b FROM abc ORDER BY a, b, c;
