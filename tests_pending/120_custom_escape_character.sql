-- PostgreSQL compatible tests from custom_escape_character
-- 16 tests

-- Test 1: query (line 2)
SELECT '%' LIKE 't%' ESCAPE CASE WHEN (SELECT '-A' SIMILAR TO '--A' ESCAPE '-') THEN 't' ELSE 'f' END;

-- Test 2: query (line 7)
SELECT '%' LIKE 't%' ESCAPE CASE WHEN (SELECT 'A' SIMILAR TO '-A' ESCAPE '') THEN 't' ELSE 'f' END;

-- Test 3: query (line 12)
SELECT '%bC' ILIKE 't%Bc' ESCAPE CASE WHEN (SELECT 'A' ILIKE '-a' ESCAPE '-') THEN 't' ELSE 'f' END;

-- Test 4: query (line 17)
SELECT 'A' LIKE '\A' ESCAPE '\';

-- Test 5: query (line 22)
SELECT 'A' LIKE '\A' ESCAPE '';

-- Test 6: query (line 27)
SELECT '%A' LIKE '_A' ESCAPE '%';

-- Test 7: query (line 32)
SELECT '%A' LIKE '%A' ESCAPE '%';

-- Test 8: query (line 37)
SELECT '%A' LIKE '%%A' ESCAPE '%';

-- Test 9: query (line 42)
SELECT '春A' LIKE '春春_' ESCAPE '春';

-- Test 10: query (line 58)
SELECT '\A' SIMILAR TO '\A' ESCAPE '';

-- Test 11: query (line 63)
SELECT '%A' SIMILAR TO '_A' ESCAPE '%';

-- Test 12: query (line 68)
SELECT '%A' SIMILAR TO '%A' ESCAPE '%';

-- Test 13: query (line 73)
SELECT '123A_' SIMILAR TO '%A_' ESCAPE '_';

-- Test 14: query (line 78)
SELECT '123A_' SIMILAR TO '%A__' ESCAPE '_';

-- Test 15: query (line 83)
SELECT '春A' SIMILAR TO '春春_' ESCAPE '春';

-- Test 16: query (line 88)
SELECT '春A_春春' SIMILAR TO '%_春_%' ESCAPE '春';
