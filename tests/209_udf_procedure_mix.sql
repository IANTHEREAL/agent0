-- PostgreSQL compatible tests from udf_procedure_mix
-- 9 tests

-- Test 1: statement (line 1)
CREATE TABLE sums (
  i INT PRIMARY KEY,
  sum INT
);

-- Test 2: statement (line 7)
CREATE FUNCTION sum_1_to_n(n INT) RETURNS INT LANGUAGE SQL AS $$
  SELECT sum(i) FROM generate_series(1, n) g(i);
$$;

-- Test 3: statement (line 13)
CREATE PROCEDURE insert_sum(n INT) LANGUAGE SQL AS $$
  INSERT INTO sums VALUES (n, sum_1_to_n(n)) ON CONFLICT DO NOTHING;
$$;

-- Test 4: statement (line 18)
CALL insert_sum(1);
CALL insert_sum(4);
CALL insert_sum(10);

-- Test 5: query (line 23)
SELECT * FROM sums
;

-- Test 6: statement (line 31)
CREATE FUNCTION fetch_sum_1_to_n(n INT) RETURNS INT LANGUAGE SQL AS $$
  CALL insert_sum(n);
  SELECT sum FROM sums WHERE i = n;
$$;

-- Test 7: query (line 37)
SELECT fetch_sum_1_to_n(4)
;

-- Test 8: query (line 42)
SELECT fetch_sum_1_to_n(9)
;

-- Test 9: query (line 47)
SELECT * FROM sums
;
