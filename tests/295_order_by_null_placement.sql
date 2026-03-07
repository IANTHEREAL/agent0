DROP TABLE IF EXISTS t295_order_by_nulls;
CREATE TABLE t295_order_by_nulls (
    id INT PRIMARY KEY,
    ord INT,
    label TEXT
);

INSERT INTO t295_order_by_nulls (id, ord, label) VALUES
    (1, 2, 'b'),
    (2, NULL, 'n'),
    (3, 1, 'a'),
    (4, 3, 'c');

-- 1) ASC default: NULLS LAST
SELECT id, ord
FROM t295_order_by_nulls
ORDER BY ord ASC;

-- 2) DESC default: NULLS FIRST
SELECT id, ord
FROM t295_order_by_nulls
ORDER BY ord DESC;

-- 3) Explicit overrides
SELECT id, ord
FROM t295_order_by_nulls
ORDER BY ord ASC NULLS FIRST;

SELECT id, ord
FROM t295_order_by_nulls
ORDER BY ord DESC NULLS LAST;

-- 4) Window ORDER BY DESC default respects NULLS FIRST
SELECT id, ord, row_number() OVER (ORDER BY ord DESC) AS rn
FROM t295_order_by_nulls
ORDER BY id;

-- 5) Aggregate ORDER BY DESC default respects NULLS FIRST
SELECT string_agg(label, ',' ORDER BY ord DESC) AS agg_desc_default
FROM t295_order_by_nulls;

DROP TABLE t295_order_by_nulls;
