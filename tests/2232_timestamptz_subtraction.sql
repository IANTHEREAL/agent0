-- Regression test: timestamptz subtraction (#2232)
-- Covers TsTz-TsTz, Ts-TsTz, TsTz-Ts, TsTz-Date, Date-TsTz,
-- Ts-Date, Date-Ts, function calls, table context, nested expressions,
-- comparisons, NULL handling, and edge cases.

SET TIME ZONE 'UTC';

-- ================================================================
-- 1. Basic subtraction between timestamp types
-- ================================================================

-- 1a. TsTz - TsTz → interval
SELECT '2024-06-15 12:00:00+00'::timestamptz - '2024-06-15 10:00:00+00'::timestamptz AS tstz_minus_tstz;

-- 1b. Ts - TsTz → interval
SELECT '2024-06-15 12:00:00'::timestamp - '2024-06-15 10:00:00+00'::timestamptz AS ts_minus_tstz;

-- 1c. TsTz - Ts → interval
SELECT '2024-06-15 12:00:00+00'::timestamptz - '2024-06-15 10:00:00'::timestamp AS tstz_minus_ts;

-- 1d. TsTz - Date → interval
SELECT '2024-06-15 12:00:00+00'::timestamptz - '2024-06-15'::date AS tstz_minus_date;

-- 1e. Date - TsTz → interval
SELECT '2024-06-15'::date - '2024-06-15 12:00:00+00'::timestamptz AS date_minus_tstz;

-- 1f. Ts - Date → interval
SELECT '2024-06-15 12:00:00'::timestamp - '2024-06-15'::date AS ts_minus_date;

-- 1g. Date - Ts → interval
SELECT '2024-06-15'::date - '2024-06-15 12:00:00'::timestamp AS date_minus_ts;

-- 1h. Negative result
SELECT '2024-06-15 10:00:00+00'::timestamptz - '2024-06-15 12:00:00+00'::timestamptz AS negative_interval;

-- 1i. Zero result
SELECT '2024-06-15 12:00:00+00'::timestamptz - '2024-06-15 12:00:00+00'::timestamptz AS zero_interval;

-- 1j. Multi-day span
SELECT '2024-06-20 12:00:00+00'::timestamptz - '2024-06-15 12:00:00+00'::timestamptz AS five_days;

-- ================================================================
-- 2. Function call subtraction
-- ================================================================

-- 2a. NOW() - NOW() within same statement is stable → 00:00:00
SELECT NOW() - NOW() AS now_minus_now;

-- 2b. CURRENT_TIMESTAMP - CURRENT_TIMESTAMP → 00:00:00
SELECT CURRENT_TIMESTAMP - CURRENT_TIMESTAMP AS ct_minus_ct;

-- 2c. Verify result type is interval
SELECT pg_typeof(NOW() - NOW()) AS result_type;

-- ================================================================
-- 3. Table context
-- ================================================================

CREATE TABLE tstz_sub_events (
    id INT PRIMARY KEY,
    started_at TIMESTAMPTZ NOT NULL,
    finished_at TIMESTAMPTZ NOT NULL
);

INSERT INTO tstz_sub_events VALUES
    (1, '2024-01-15 08:00:00+00', '2024-01-15 09:30:00+00'),
    (2, '2024-01-15 10:00:00+00', '2024-01-15 14:00:00+00'),
    (3, '2024-01-15 20:00:00+00', '2024-01-16 02:00:00+00');

-- 3a. SELECT subtraction from columns
SELECT id, finished_at - started_at AS duration
FROM tstz_sub_events ORDER BY id;

-- 3b. WHERE clause with subtraction
SELECT id
FROM tstz_sub_events
WHERE finished_at - started_at > INTERVAL '2 hours'
ORDER BY id;

-- 3c. ORDER BY subtraction
SELECT id
FROM tstz_sub_events
ORDER BY finished_at - started_at DESC;

-- ================================================================
-- 4. Nested expressions
-- ================================================================

-- 4a. EXTRACT(EPOCH FROM tstz - tstz) → seconds
SELECT EXTRACT(EPOCH FROM '2024-06-15 12:00:00+00'::timestamptz - '2024-06-15 10:00:00+00'::timestamptz) AS epoch_diff;

-- 4b. EXTRACT on table columns
SELECT id, EXTRACT(EPOCH FROM finished_at - started_at) AS duration_secs
FROM tstz_sub_events ORDER BY id;

-- 4c. ABS(EXTRACT(EPOCH FROM ...)) with negative difference
SELECT ABS(EXTRACT(EPOCH FROM '2024-06-15 10:00:00+00'::timestamptz - '2024-06-15 12:00:00+00'::timestamptz)) AS abs_epoch;

-- ================================================================
-- 5. Comparison with interval
-- ================================================================

SELECT id FROM tstz_sub_events
WHERE finished_at - started_at > INTERVAL '1 hour'
ORDER BY id;

SELECT id FROM tstz_sub_events
WHERE finished_at - started_at <= INTERVAL '1 hour 30 minutes'
ORDER BY id;

SELECT id FROM tstz_sub_events
WHERE finished_at - started_at BETWEEN INTERVAL '1 hour' AND INTERVAL '5 hours'
ORDER BY id;

-- ================================================================
-- 6. NULL handling
-- ================================================================

SELECT NULL::timestamptz - NOW() IS NULL AS is_null;
SELECT NOW() - NULL::timestamptz IS NULL AS is_null;
SELECT (NULL::timestamptz - NULL::timestamptz) IS NULL AS is_null;
SELECT COALESCE(NULL::timestamptz - NOW(), INTERVAL '0 seconds') AS coalesced;

-- ================================================================
-- 7. Edge cases
-- ================================================================

-- 7a. Same instant different offsets
SELECT '2024-06-15 12:00:00+00'::timestamptz - '2024-06-15 07:00:00-05'::timestamptz AS same_instant;

-- 7b. Different instants different offsets
SELECT '2024-06-15 12:00:00+00'::timestamptz - '2024-06-15 12:00:00+05'::timestamptz AS tz_diff;

-- 7c. Epoch boundary
SELECT '1970-01-01 00:00:01+00'::timestamptz - '1970-01-01 00:00:00+00'::timestamptz AS one_sec;

-- 7d. Pre-epoch crossing
SELECT '1970-01-01 00:00:00+00'::timestamptz - '1969-12-31 23:59:59+00'::timestamptz AS pre_epoch;

-- 7e. Large span with leap year
SELECT '2024-01-01 00:00:00+00'::timestamptz - '2020-01-01 00:00:00+00'::timestamptz AS four_years;

-- ================================================================
-- 8. Date ± Int64
-- ================================================================

SELECT DATE '2024-01-10' + CAST(5 AS BIGINT) AS date_plus_bigint;
SELECT DATE '2024-01-10' - CAST(5 AS BIGINT) AS date_minus_bigint;

-- ================================================================
-- 9. Month-based interval comparisons (to_millis_approx coverage)
-- ================================================================

-- 9a. Month vs month
SELECT '2 months'::interval > '1 month'::interval AS two_gt_one;

-- 9b. Month vs days (30 days/month normalization)
SELECT '1 month'::interval > '31 days'::interval AS month_gt_31d;
SELECT '1 month'::interval = '30 days'::interval AS month_eq_30d;
SELECT '1 month'::interval < '29 days'::interval AS month_lt_29d;

-- 9c. Mixed month + time vs pure days
SELECT '1 month 1 day'::interval > '30 days'::interval AS mon1d_gt_30d;

-- 9d. Negative month comparison
SELECT '-1 month'::interval < '0 seconds'::interval AS neg_lt_zero;

-- 9e. ORDER BY with month intervals
SELECT v FROM (VALUES ('2 months'::interval), ('59 days'::interval), ('61 days'::interval), ('1 month'::interval)) AS t(v) ORDER BY v;

-- ================================================================
-- 10. EXTRACT from negative intervals (sign preservation)
-- ================================================================

-- Use subtraction to produce negative intervals (parser doesn't handle '-HH:MM:SS')
SELECT EXTRACT(EPOCH FROM '2024-06-15 10:00:00+00'::timestamptz - '2024-06-15 12:00:00+00'::timestamptz) AS neg_epoch;
SELECT EXTRACT(EPOCH FROM '2024-06-14 00:00:00+00'::timestamptz - '2024-06-15 00:00:00+00'::timestamptz) AS neg_day_epoch;

-- ================================================================
-- Cleanup
-- ================================================================

DROP TABLE tstz_sub_events;
