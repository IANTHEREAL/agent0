-- PG_PARITY: PostgreSQL-compatible math function behavior.
SELECT ABS(-42) AS abs_neg;
SELECT ABS(42) AS abs_pos;

SELECT CEIL(4.2) AS ceil_pos;
SELECT CEIL(-4.2) AS ceil_neg;
SELECT FLOOR(4.8) AS floor_pos;
SELECT FLOOR(-4.8) AS floor_neg;

SELECT ROUND(4.5) AS round_half;
SELECT ROUND(4.4) AS round_down;
SELECT ROUND(4.567, 2) AS round_2dp;
SELECT TRUNC(4.567) AS trunc_int;
SELECT TRUNC(4.567, 2) AS trunc_2dp;

SELECT SQRT(16) AS sqrt_16;
SELECT SQRT(2) AS sqrt_2;
SELECT CBRT(27) AS cbrt_27;
SELECT POWER(2, 10) AS power_2_10;

SELECT EXP(1) AS exp_1;
SELECT LN(2.718281828) AS ln_e;
SELECT LOG(100) AS log_100;

SELECT MOD(17, 5) AS mod_17_5;
SELECT MOD(-17, 5) AS mod_neg;
SELECT 17 % 5 AS modulo_op;

SELECT SIGN(42) AS sign_pos;
SELECT SIGN(-42) AS sign_neg;
SELECT SIGN(0) AS sign_zero;
SELECT PG_TYPEOF(SIGN(42)) AS sign_int_type;
SELECT PG_TYPEOF(SIGN(CAST(42 AS BIGINT))) AS sign_bigint_type;
SELECT PG_TYPEOF(SIGN(CAST(42 AS DOUBLE PRECISION))) AS sign_float_type;
SELECT PG_TYPEOF(SIGN(CAST(42 AS NUMERIC))) AS sign_numeric_type;

-- SIGN per PostgreSQL 17.7: integer/bigint/double precision inputs
-- resolve to double precision; explicit numeric input returns bare numeric.
SELECT PG_TYPEOF(SIGN(1::integer)) AS sign_typeof_int;
SELECT PG_TYPEOF(SIGN(1::bigint)) AS sign_typeof_bigint;
SELECT PG_TYPEOF(SIGN(1::double precision)) AS sign_typeof_double;
SELECT PG_TYPEOF(SIGN(1.5::numeric)) AS sign_typeof_numeric;
-- SIGN(numeric(p,s)) returns bare numeric, with no typmod leak.
SELECT PG_TYPEOF(SIGN(1.5::numeric(10,2))) AS sign_typeof_numeric_typmod;

-- Typed PREPARE paths should preserve the PG SIGN contract and value
-- behavior, not just literal happy paths.
PREPARE sign_prep_numeric(numeric) AS SELECT SIGN($1) AS v;
PREPARE sign_prep_int(int)       AS SELECT SIGN($1) AS v;
PREPARE sign_prep_bigint(bigint) AS SELECT SIGN($1) AS v;
EXECUTE sign_prep_numeric(2.5);
EXECUTE sign_prep_int(-7);
EXECUTE sign_prep_bigint(0::bigint);
DEALLOCATE sign_prep_numeric;
DEALLOCATE sign_prep_int;
DEALLOCATE sign_prep_bigint;

SELECT GREATEST(1, 5, 3, 9, 2) AS greatest;
SELECT LEAST(1, 5, 3, 9, 2) AS least;

SELECT PI() AS pi_value;

SELECT DEGREES(PI()) AS degrees_pi;
SELECT RADIANS(180) AS radians_180;
SELECT SIN(0) AS sin_0;
SELECT COS(0) AS cos_0;
SELECT TAN(0) AS tan_0;

SELECT 5 + 3 AS add;
SELECT 10 - 4 AS subtract;
SELECT 6 * 7 AS multiply;
SELECT 15 / 4 AS int_divide;
SELECT 15.0 / 4 AS float_divide;

SELECT 'Math function tests completed' AS result;
