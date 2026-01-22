-- PostgreSQL compatible tests from udf_prepare
-- 14 tests

-- Test 1: statement (line 1)
CREATE FUNCTION f() RETURNS INT LANGUAGE SQL AS $$ SELECT 1 $$;

-- Test 2: statement (line 4)
PREPARE p AS SELECT $1::INT;

-- Test 3: statement (line 7)
EXECUTE p(f());

-- Test 4: statement (line 10)
DEALLOCATE p;

-- Test 5: statement (line 16)
CREATE FUNCTION f147186() RETURNS INT LANGUAGE SQL AS $$ SELECT CAST(current_setting('foo.bar') AS INT) $$;

-- Test 6: statement (line 19)
CREATE TABLE t147186 (a INT, b INT DEFAULT f147186());

-- Test 7: statement (line 22)
PREPARE p AS INSERT INTO t147186 (a) VALUES ($1);

-- Test 8: statement (line 25)
SET foo.bar = '100';

-- Test 9: statement (line 28)
EXECUTE p(1);

-- Test 10: query (line 31)
SELECT a, b FROM t147186;

-- Test 11: statement (line 36)
SET foo.bar = '200';

-- Test 12: statement (line 39)
EXECUTE p(2);

-- Test 13: query (line 43)
SELECT a, b FROM t147186;

-- Test 14: statement (line 49)
DEALLOCATE p;
