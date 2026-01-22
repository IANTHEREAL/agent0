-- PostgreSQL compatible tests from group_join
-- 4 tests

-- Test 1: statement (line 1)
CREATE TABLE data (a INT, b INT, c INT, d INT, PRIMARY KEY (a, b, c, d));

-- Test 2: statement (line 5)
INSERT INTO data SELECT a, b, c, d FROM
   generate_series(1, 3) AS a(a),
   generate_series(1, 3) AS b(b),
   generate_series(1, 3) AS c(c),
   generate_series(1, 3) AS d(d);

-- Test 3: query (line 15)
SELECT data1.a, sum(data1.d) FROM data AS data1 INNER JOIN data AS data2 ON data1.a = data2.c GROUP BY data1.a;

-- Test 4: query (line 26)
SELECT data1.a, sum(data1.d) FROM data AS data1 INNER JOIN data AS data2 ON data1.a = data2.c GROUP BY data1.a;
