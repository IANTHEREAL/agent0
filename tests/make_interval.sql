-- Regression test: make_interval() datetime function (#2162)
-- Covers positional args, named args, mixed defaults, and arithmetic.

-- 1. Zero-arg (all defaults) → 00:00:00
SELECT make_interval();

-- 2. Named: secs only (Plan A use case)
SELECT make_interval(secs => 120);

-- 3. Named: days + hours
SELECT make_interval(days => 1, hours => 2);

-- 4. Named: years + months
SELECT make_interval(years => 1, months => 6);

-- 5. Positional: all 7 args (years, months, weeks, days, hours, mins, secs)
SELECT make_interval(0, 0, 0, 0, 1, 30, 0);

-- 6. Named: weeks converts to days
SELECT make_interval(weeks => 2);

-- 7. Named: secs with whole number (integer-safe)
SELECT make_interval(secs => 90);

-- 8. Arithmetic: now() + make_interval(secs => 60) should not error
SELECT CASE WHEN now() + make_interval(secs => 60) > now() THEN 'future_ok' ELSE 'future_fail' END AS result;

-- 9. Named: mins only
SELECT make_interval(mins => 45);

-- 10. Mixed positional (3 positional = years, months, weeks)
SELECT make_interval(1, 2, 3);
