-- Date type improvements (arithmetic + cross-type comparisons + functions).

-- DATE +/- INTERVAL
SELECT CURRENT_DATE + INTERVAL '1 day';
SELECT CURRENT_DATE - INTERVAL '1 month';

-- DATE - DATE returns integer days
SELECT CAST('2024-01-15' AS DATE) - CAST('2024-01-10' AS DATE);

-- DATE vs TIMESTAMP comparison in WHERE
DROP TABLE IF EXISTS date_type_events;
CREATE TABLE date_type_events (id INT PRIMARY KEY, event_date DATE);
INSERT INTO date_type_events(id, event_date) VALUES (1, CURRENT_DATE);
SELECT id FROM date_type_events WHERE event_date < NOW() ORDER BY id;
DROP TABLE date_type_events;

-- DATE(timestamp) returns DATE
DROP TABLE IF EXISTS date_type_ts;
CREATE TABLE date_type_ts (id INT PRIMARY KEY, ts TIMESTAMP);
INSERT INTO date_type_ts(id, ts) VALUES (1, NOW());
SELECT DATE(ts) FROM date_type_ts ORDER BY id;
DROP TABLE date_type_ts;

-- AGE(date, date) is non-NULL
DROP TABLE IF EXISTS date_type_birth;
CREATE TABLE date_type_birth (id INT PRIMARY KEY, birth_date DATE);
INSERT INTO date_type_birth(id, birth_date) VALUES (1, '2000-01-01');
SELECT AGE(CURRENT_DATE, birth_date) FROM date_type_birth ORDER BY id;
DROP TABLE date_type_birth;

