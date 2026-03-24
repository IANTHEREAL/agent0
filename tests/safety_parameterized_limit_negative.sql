-- Regression guard for PR #2022: parameterized LIMIT/OFFSET boundary cases.

DROP TABLE IF EXISTS plim_neg;
CREATE TABLE plim_neg (id INT PRIMARY KEY);
INSERT INTO plim_neg VALUES (1), (2), (3), (4), (5);

-- ================================================================
-- Test 1: LIMIT 0 returns zero rows
-- ================================================================
PREPARE p_lim0(int) AS SELECT id FROM plim_neg ORDER BY id LIMIT $1;
EXECUTE p_lim0(0);
DEALLOCATE p_lim0;

-- ================================================================
-- Test 2: Negative LIMIT must error
-- ================================================================
PREPARE p_lim_neg(int) AS SELECT id FROM plim_neg ORDER BY id LIMIT $1;
EXECUTE p_lim_neg(-1);
DEALLOCATE p_lim_neg;

-- ================================================================
-- Test 3: Negative OFFSET must error
-- ================================================================
PREPARE p_off_neg(int, int) AS SELECT id FROM plim_neg ORDER BY id LIMIT $1 OFFSET $2;
EXECUTE p_off_neg(3, -1);
DEALLOCATE p_off_neg;

-- ================================================================
-- Test 4: OFFSET 0 = no skip
-- ================================================================
PREPARE p_off0(int, int) AS SELECT id FROM plim_neg ORDER BY id LIMIT $1 OFFSET $2;
EXECUTE p_off0(2, 0);
DEALLOCATE p_off0;

DROP TABLE plim_neg;
