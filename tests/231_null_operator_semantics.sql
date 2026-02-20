-- NULL operator semantics regression tests
-- Verifies strict operators return NULL when either input is NULL.
-- Expected outputs verified against PostgreSQL 17.7.

-- Regex operators with NULL (strict — must return NULL)
SELECT NULL ~ 'foo' AS regex_null_left;
SELECT 'abc' ~ NULL AS regex_null_right;
SELECT NULL !~ 'foo' AS regex_not_null_left;
SELECT NULL ~* 'foo' AS regex_ci_null_left;
SELECT NULL !~* 'foo' AS regex_nci_null_left;

-- Text concat with NULL (strict for text operands)
SELECT 'hello' || NULL AS concat_null_right;
SELECT NULL || 'world' AS concat_null_left;

-- Arithmetic with NULL (strict — must return NULL)
SELECT NULL + 1 AS plus_null;
SELECT 2 - NULL AS minus_null;
SELECT NULL * 3 AS mul_null;
SELECT 4 / NULL AS div_null;
SELECT NULL % 5 AS mod_null;

-- Comparison with NULL (strict — must return NULL)
SELECT 1 = NULL AS eq_null;
SELECT NULL <> 2 AS neq_null;
SELECT NULL > 0 AS gt_null;
SELECT 0 < NULL AS lt_null;
SELECT NULL >= 1 AS gte_null;
SELECT 1 <= NULL AS lte_null;
