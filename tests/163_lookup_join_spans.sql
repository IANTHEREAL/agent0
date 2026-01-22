-- PostgreSQL compatible tests from lookup_join_spans
-- 37 tests

SET client_min_messages = warning;

-- Test 1: statement (line 10)
CREATE TABLE metrics (
  id       BIGINT PRIMARY KEY,
  nullable INT,
  name     TEXT
);
INSERT INTO metrics (id, nullable, name)
VALUES (1, NULL, 'cpu'), (2, 1, 'cpu'), (3, NULL, 'mem'), (4, 2, 'disk');

-- Test 2: statement (line 13)
CREATE TABLE metric_values (
  metric_id BIGINT,
  time      TIMESTAMPTZ,
  nullable  INT,
  value     BIGINT,
  PRIMARY KEY (metric_id, time)
);
CREATE INDEX metric_values_secondary ON metric_values (metric_id, nullable, time);

-- Test 3: statement (line 23)
insert into metric_values (metric_id, time, nullable, value) values
 (1,'2020-01-01 00:00:00+00:00',NULL,0),
 (1,'2020-01-01 00:00:01+00:00',1,1),
 (2,'2020-01-01 00:00:00+00:00',NULL,2),
 (2,'2020-01-01 00:00:01+00:00',2,3),
 (2,'2020-01-01 00:01:01+00:00',-11,4),
 (2,'2020-01-01 00:01:02+00:00',-10,5),
 (3,'2020-01-01 00:01:00+00:00',NULL,6),
 (3,'2020-01-01 00:01:01+00:00',3,7);

-- Test 4: statement (line 35)
CREATE TABLE metric_values_desc (
  metric_id BIGINT,
  time      TIMESTAMPTZ,
  nullable  INT,
  value     BIGINT,
  PRIMARY KEY (metric_id, time)
);
CREATE INDEX metric_values_desc_secondary
  ON metric_values_desc (metric_id, nullable DESC, time DESC);

-- Test 5: statement (line 45)
insert into metric_values_desc select * from metric_values;

-- Test 6: statement (line 49)
-- CockroachDB-only; in PostgreSQL we just compute planner stats normally.
ANALYZE metric_values;
ANALYZE metric_values_desc;
ANALYZE metrics;

-- Test 8: query (line 106)
SELECT *
FROM metric_values as v
INNER JOIN metrics as m
ON metric_id=id
WHERE
  time > '2020-01-01 00:00:00+00:00' AND
  name='cpu'
ORDER BY value;

-- Test 9: query (line 121)
SELECT *
FROM metric_values_desc
INNER JOIN metrics
ON metric_id=id
WHERE
  time > '2020-01-01 00:00:00+00:00' AND
  name='cpu'
ORDER BY value;

-- Test 10: query (line 136)
SELECT *
FROM metric_values
INNER JOIN metrics
ON metric_id=id
WHERE
  time >= '2020-01-01 00:00:00+00:00' AND
  name='cpu'
ORDER BY value;

-- Test 11: query (line 153)
SELECT *
FROM metric_values_desc
INNER JOIN metrics
ON metric_id=id
WHERE
  time >= '2020-01-01 00:00:00+00:00' AND
  name='cpu'
ORDER BY value;

-- Test 12: query (line 170)
SELECT *
FROM metric_values
INNER JOIN metrics
ON metric_id=id
WHERE
  time < '2020-01-01 00:00:00+00:00' AND
  name='cpu';

-- Test 13: query (line 180)
SELECT *
FROM metric_values_desc
INNER JOIN metrics
ON metric_id=id
WHERE
  time < '2020-01-01 00:00:00+00:00' AND
  name='cpu';

-- Test 14: query (line 190)
SELECT *
FROM metric_values
INNER JOIN metrics
ON metric_id=id
WHERE
  time <= '2020-01-01 00:00:00+00:00' AND
  name='cpu'
ORDER BY value;

-- Test 15: query (line 203)
SELECT *
FROM metric_values_desc
INNER JOIN metrics
ON metric_id=id
WHERE
  time <= '2020-01-01 00:00:00+00:00' AND
  name='cpu'
ORDER BY value;

-- Test 16: query (line 216)
SELECT *
FROM metric_values
INNER JOIN metrics
ON metric_id=id
WHERE
  time < '2020-01-01 00:00:10+00:00' AND
  name='cpu'
ORDER BY value;

-- Test 17: query (line 231)
SELECT *
FROM metric_values_desc
INNER JOIN metrics
ON metric_id=id
WHERE
  time < '2020-01-01 00:00:10+00:00' AND
  name='cpu'
ORDER BY value;

-- Test 18: query (line 246)
SELECT *
FROM metric_values
INNER JOIN metrics
ON metric_id=id
WHERE
  time BETWEEN '2020-01-01 00:00:00+00:00' AND '2020-01-01 00:10:00+00:00' AND
  name='cpu'
ORDER BY value;

-- Test 19: query (line 263)
SELECT *
FROM metric_values_desc
INNER JOIN metrics
ON metric_id=id
WHERE
  time BETWEEN '2020-01-01 00:00:00+00:00' AND '2020-01-01 00:10:00+00:00' AND
  name='cpu'
ORDER BY value;

-- Test 20: query (line 281)
SELECT *
FROM metrics
LEFT JOIN metric_values
ON metric_id=id
AND time BETWEEN '2020-01-01 00:00:00+00:00' AND '2020-01-01 00:10:00+00:00'
AND name='cpu'
ORDER BY value, id;

-- Test 21: query (line 300)
SELECT *
FROM metrics m
WHERE EXISTS (SELECT * FROM metric_values mv WHERE mv.metric_id = m.id AND time BETWEEN '2020-01-01 00:00:00+00:00' AND '2020-01-01 00:10:00+00:00')
ORDER BY m.id;

-- Test 22: query (line 311)
SELECT *
FROM metric_values as v
INNER JOIN metrics as m
ON metric_id=id
AND v.nullable = m.nullable
WHERE
  time > '2020-01-01 00:00:00+00:00' AND
  name='cpu'
ORDER BY value;

-- Test 23: query (line 324)
SELECT *
FROM metric_values as v
INNER JOIN metrics as m
ON metric_id=id
WHERE
  v.nullable BETWEEN -20 AND -10 AND
  name='cpu'
ORDER BY value;

-- Test 24: query (line 338)
SELECT *
FROM metric_values_desc as v
INNER JOIN metrics as m
ON metric_id=id
WHERE
  v.nullable BETWEEN -20 AND -10 AND
  name='cpu'
ORDER BY value;

-- Test 25: query (line 352)
SELECT *
FROM metric_values as v
INNER JOIN metrics as m
ON metric_id=id
WHERE
  v.nullable > 1 AND
  name='cpu'
ORDER BY value;

-- Test 26: query (line 365)
SELECT *
FROM metric_values_desc as v
INNER JOIN metrics as m
ON metric_id=id
WHERE
  v.nullable > 1 AND
  name='cpu'
ORDER BY value;

-- Test 27: query (line 378)
SELECT *
FROM metric_values as v
INNER JOIN metrics as m
ON metric_id=id
WHERE
  v.nullable >= 1 AND
  name='cpu'
ORDER BY value;

-- Test 28: query (line 392)
SELECT *
FROM metric_values_desc as v
INNER JOIN metrics as m
ON metric_id=id
WHERE
  v.nullable >= 1 AND
  name='cpu'
ORDER BY value;

-- Test 29: query (line 406)
SELECT *
FROM metric_values as v
INNER JOIN metrics as m
ON metric_id=id
WHERE
  v.nullable < -10 AND
  name='cpu'
ORDER BY value;

-- Test 30: query (line 419)
SELECT *
FROM metric_values_desc as v
INNER JOIN metrics as m
ON metric_id=id
WHERE
  v.nullable < -10 AND
  name='cpu'
ORDER BY value;

-- Test 31: query (line 432)
SELECT *
FROM metric_values as v
INNER JOIN metrics as m
ON metric_id=id
WHERE
  v.nullable <= -10 AND
  name='cpu'
ORDER BY value;

-- Test 32: query (line 446)
SELECT *
FROM metric_values_desc as v
INNER JOIN metrics as m
ON metric_id=id
WHERE
  v.nullable <= -10 AND
  name='cpu'
ORDER BY value;

-- Test 33: query (line 460)
SELECT *
FROM metric_values as v
INNER JOIN metrics as m
ON metric_id=id
WHERE
  time < '2020-01-01 00:00:10+00:00' AND
  name='cpu' AND
  v.nullable = m.nullable
ORDER BY value;

-- Test 34: query (line 473)
SELECT *
FROM metric_values as v
INNER JOIN metrics as m
ON metric_id=id
WHERE
  time < '2020-01-01 00:00:10+00:00' AND
  name='cpu' AND
  v.nullable = 1
ORDER BY value;

-- Test 35: statement (line 488)
CREATE TABLE order_line (ol_o_id INT8, ol_i_id INT8);
INSERT
INTO
  order_line (ol_o_id, ol_i_id)
VALUES
  (19, 6463), (20, 6463), (100, 6463), (101, 6463);
CREATE INDEX ol_io ON order_line (ol_i_id, ol_o_id);
CREATE TABLE stock (s_i_id INT8);
INSERT INTO stock (s_i_id) VALUES (6463);

-- Test 36: query (line 499)
SELECT
  s_i_id, ol_o_id
FROM
  stock INNER JOIN order_line ON s_i_id = ol_i_id
WHERE
  ol_o_id BETWEEN 20 AND 100;

-- Test 37: query (line 511)
SELECT
  s_i_id, ol_o_id
FROM
  stock INNER JOIN order_line ON s_i_id = ol_i_id
WHERE
  ol_o_id IN (19, 20, 21) AND ol_o_id >= 20;

RESET client_min_messages;
