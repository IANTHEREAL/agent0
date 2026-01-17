-- Auto tests: Types and Expressions
CREATE SCHEMA IF NOT EXISTS auto_tests;
SET search_path TO auto_tests, public;

DROP TABLE IF EXISTS types_demo;

CREATE TABLE types_demo (
    id INT PRIMARY KEY,
    u UUID,
    d DATE,
    ts TIMESTAMP,
    j JSONB,
    n NUMERIC(10,2)
);

INSERT INTO types_demo (id, u, d, ts, j, n) VALUES
    (1, '550e8400-e29b-41d4-a716-446655440000', '2020-01-02', '2020-01-02 10:00:00', '{"type": "click", "count": 3}', 123.45);

SELECT id,
       u,
       d,
       ts,
       j->>'type' AS j_type,
       j->'count' AS j_count,
       n,
       CASE WHEN n > 100 THEN 'high' ELSE 'low' END AS bucket
FROM types_demo;

SELECT ts + INTERVAL '1 day' AS ts_plus_one FROM types_demo;

DROP TABLE types_demo;
