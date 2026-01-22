-- PostgreSQL compatible tests from impure
-- 17 tests

-- Test 1: query (line 4)
WITH cte (a, b) AS (SELECT random(), random())
SELECT count(*) FROM cte WHERE a = b;

-- Test 2: query (line 10)
WITH cte (x, a, b) AS (SELECT x, random(), random() FROM (VALUES (1), (2), (3)) AS v(x))
SELECT count(*) FROM cte WHERE a = b;

-- Test 3: statement (line 16)
CREATE TABLE kab (k INT PRIMARY KEY, a UUID, b UUID);

-- Test 4: statement (line 19)
INSERT INTO kab VALUES (1, gen_random_uuid(), gen_random_uuid());

-- Test 5: statement (line 22)
INSERT INTO kab VALUES (2, gen_random_uuid(), gen_random_uuid());

-- Test 6: statement (line 25)
INSERT INTO kab VALUES (3, gen_random_uuid(), gen_random_uuid()),
                       (4, gen_random_uuid(), gen_random_uuid()),
                       (5, gen_random_uuid(), gen_random_uuid()),
                       (6, gen_random_uuid(), gen_random_uuid());

-- Test 7: query (line 31)
SELECT count(*) FROM kab WHERE a=b;

-- Test 8: statement (line 36)
CREATE TABLE kabc (k INT PRIMARY KEY, a UUID, b UUID);

-- Test 9: statement (line 39)
INSERT INTO kabc VALUES (1, gen_random_uuid(), gen_random_uuid());

-- Test 10: statement (line 42)
INSERT INTO kabc VALUES (2, gen_random_uuid(), gen_random_uuid());

-- Test 11: statement (line 45)
INSERT INTO kabc VALUES (3, gen_random_uuid(), gen_random_uuid()),
                       (4, gen_random_uuid(), gen_random_uuid()),
                       (5, gen_random_uuid(), gen_random_uuid()),
                       (6, gen_random_uuid(), gen_random_uuid());

-- Test 12: query (line 51)
SELECT count(*) FROM kabc WHERE a=b;

-- Test 13: statement (line 56)
CREATE TABLE kabcd (
  k INT PRIMARY KEY,
  a UUID,
  b UUID,
  c UUID DEFAULT gen_random_uuid(),
  d UUID DEFAULT gen_random_uuid(),
  e UUID DEFAULT gen_random_uuid(),
  f UUID DEFAULT gen_random_uuid()
);

-- Test 14: statement (line 67)
INSERT INTO kabcd VALUES (1, gen_random_uuid(), gen_random_uuid());

-- Test 15: statement (line 70)
INSERT INTO kabcd VALUES (2, gen_random_uuid(), gen_random_uuid());

-- Test 16: statement (line 73)
INSERT INTO kabcd VALUES (3, gen_random_uuid(), gen_random_uuid()),
                         (4, gen_random_uuid(), gen_random_uuid()),
                         (5, gen_random_uuid(), gen_random_uuid()),
                         (6, gen_random_uuid(), gen_random_uuid());

-- Test 17: query (line 79)
SELECT count(*) FROM kabcd WHERE a=b OR a=c OR a=d OR a=e OR a=f
OR b=c OR b=d OR b=e OR b=f OR c=d OR c=e OR c=f OR d=e OR d=f OR e=f;
