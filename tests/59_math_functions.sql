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
