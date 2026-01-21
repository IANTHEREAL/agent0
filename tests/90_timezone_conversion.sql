-- PRD-D01: AT TIME ZONE timezone conversion (Dify compatibility)

-- timestamp -> timestamptz (interpret timestamp in the given zone)
SELECT TIMESTAMP '2024-01-15 10:00:00' AT TIME ZONE 'UTC' AS ts_utc;
SELECT TIMESTAMP '2024-01-15 10:00:00' AT TIME ZONE 'Asia/Shanghai' AS ts_shanghai;
SELECT TIMESTAMP '2024-01-15 10:00:00' AT TIME ZONE '+08:00' AS ts_offset;
SELECT TIMESTAMP '2024-01-15 10:00:00' AT TIME ZONE '-05:00' AS ts_minus_5;

-- chained conversion (timestamp -> timestamptz -> timestamp)
SELECT TIMESTAMP '2024-01-15 10:00:00'
    AT TIME ZONE 'UTC'
    AT TIME ZONE 'America/New_York' AS ts_ny;

-- timestamptz -> timestamp (convert instant into the given zone)
SELECT TIMESTAMPTZ '2024-01-15T10:00:00Z' AT TIME ZONE 'America/New_York' AS tz_to_ts;

-- Dify pattern: day bucketing in target zone
SELECT DATE(DATE_TRUNC('day',
    TIMESTAMP '2024-01-15 10:30:00' AT TIME ZONE 'UTC' AT TIME ZONE 'America/New_York'
)) AS dify_date;

