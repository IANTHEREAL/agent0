-- Regression guard for PR #2028: math function domain errors.
-- ln(0), ln(-1), log(0), log(-1), log10(0), log10(-1) must return
-- SQLSTATE 2201E (invalid_argument_for_logarithm), not crash.

-- ================================================================
-- Error cases
-- ================================================================
SELECT ln(0::numeric);
SELECT ln(-1::numeric);
SELECT log(0::numeric);
SELECT log(-1::numeric);

-- ================================================================
-- Normal cases (must succeed)
-- ================================================================
SELECT round(ln(2.718281828::numeric), 4);
SELECT round(log(100::numeric), 4);

-- ================================================================
-- Cleanup (no tables created)
-- ================================================================
