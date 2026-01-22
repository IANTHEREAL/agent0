-- PostgreSQL compatible tests from apply_join
-- 45 tests

SET client_min_messages = warning;

-- Test 1: statement (line 10)
-- SET allow_prepare_as_opt_plan = ON

DROP TABLE IF EXISTS t;
DROP TABLE IF EXISTS u;
DROP TABLE IF EXISTS v;
DROP TABLE IF EXISTS table9;
DROP TABLE IF EXISTS seed;

CREATE TABLE t (k INT, str TEXT);
CREATE TABLE u (l INT, str2 TEXT);
CREATE TABLE v (m INT, str3 TEXT);

INSERT INTO t VALUES (1, 't1'), (2, 't2'), (3, 't3');
INSERT INTO u VALUES (2, 'u2'), (3, 'u3'), (4, 'u4');
INSERT INTO v VALUES (1, 'v1'), (2, 'v2'), (3, 'v3');

CREATE TABLE table9 (
    _string TEXT,
    _float8 DOUBLE PRECISION,
    _float4 REAL,
    _interval INTERVAL,
    _uuid UUID
);

INSERT INTO table9 VALUES
    ('a', 1.5, 1.5, INTERVAL '1 day', '00000000-0000-0000-0000-000000000001'),
    ('b', 2.5, 2.5, INTERVAL '2 days', '00000000-0000-0000-0000-000000000002');

CREATE TABLE seed (
    _int8 BIGINT DEFAULT 0,
    _float8 DOUBLE PRECISION DEFAULT 0,
    _date DATE DEFAULT CURRENT_DATE,
    _jsonb JSONB DEFAULT '{}'::jsonb
);

-- Test 2: statement (line 15)
PREPARE a AS
SELECT t.k, t.str, u.l, u.str2
FROM t
JOIN u ON u.l = t.k;

-- Test 3: query (line 31)
EXECUTE a;

-- Test 4: statement (line 41)
PREPARE right_no_cols AS
SELECT t.k, t.str
FROM t
WHERE t.k = 2;

-- Test 5: query (line 57)
EXECUTE right_no_cols;

-- Test 6: statement (line 68)
PREPARE b AS
SELECT t.k, t.str, u.l, u.str2
FROM t
LEFT JOIN u ON u.l = t.k + 1;

-- Test 7: query (line 84)
EXECUTE b;

-- Test 8: statement (line 95)
PREPARE c AS
SELECT t.k, t.str
FROM t
WHERE EXISTS (
    SELECT 1
    FROM u
    WHERE u.l = t.k + 1
);

-- Test 9: query (line 111)
EXECUTE c;

-- Test 10: statement (line 121)
PREPARE d AS
SELECT t.k, t.str
FROM t
WHERE NOT EXISTS (
    SELECT 1
    FROM u
    WHERE u.l = t.k + 1
);

-- Test 11: query (line 137)
EXECUTE d;

-- Test 12: statement (line 145)
PREPARE e AS
SELECT t.k, t.str, u.l, u.str2, v.m, v.str3
FROM t
JOIN u ON u.l = t.k
JOIN v ON v.m = t.k;

-- Test 13: query (line 169)
EXECUTE e;

-- Test 14: statement (line 180)
PREPARE f AS
SELECT t.k, t.str, u.l, u.str2
FROM t
JOIN u ON u.l = t.k + 1;

-- Test 15: query (line 200)
EXECUTE f;

-- Test 16: query (line 210)
SELECT
	(SELECT * FROM (VALUES ((SELECT x FROM (VALUES (1)) AS s (x)) + y)))
FROM
	(VALUES (1), (2), (3)) AS t (y);

-- Test 17: query (line 243)
SELECT
  true
FROM
    table9 AS tab_27927
WHERE
    EXISTS(
        SELECT
            tab_27929._string AS col_85223
        FROM
            table9 AS tab_27928,
            table9 AS tab_27929,
            table9 AS tab_27930
            RIGHT JOIN table9 AS tab_27931
            ON
                NOT
                    (
                        tab_27927._float8
                        IN (
                                tab_27927._float4,
                                tab_27927._float4,
                                tab_27927._float8::FLOAT8
                                + NULL::FLOAT8,
                                tab_27927._float4
                            )
                    )
        WHERE
            EXISTS(
                SELECT
                    2470039497::OID AS col_85224
                FROM
                    table9 AS tab_27932
                ORDER BY
                    tab_27932._string ASC,
                    tab_27932._interval DESC,
                    tab_27932._uuid DESC
                LIMIT
                    37::BIGINT
            )
        LIMIT
            11::BIGINT
    )
LIMIT
    89::BIGINT;

-- Test 18: statement (line 299)
CREATE TABLE x (a BIGINT);
CREATE TABLE y (b BIGINT);
INSERT INTO x VALUES (1);
INSERT INTO y VALUES (2);

-- Test 19: query (line 302)
SELECT a, (SELECT b FROM y) FROM x;

-- Test 20: statement (line 308)
CREATE TABLE IF NOT EXISTS t40589 AS
	SELECT
		'2001-01-01'::TIMESTAMPTZ + g * INTERVAL '1 day',
		g * INTERVAL '1 day' AS _interval,
		g % 1 = 0 AS _bool,
		g::DECIMAL AS _decimal,
		g,
		convert_to(g::TEXT, 'UTF8') AS _bytes,
		substring(NULL, NULL, NULL)::UUID AS _uuid,
		'0.0.0.0'::INET + g AS _inet,
		to_jsonb(g) AS _jsonb
	FROM
		generate_series(NULL::BIGINT, NULL::BIGINT) AS g;

-- Test 21: query (line 323)
SELECT
	(
		SELECT NULL
		FROM t40589 AS t0
		LIMIT 1
	)
FROM
	t40589 AS t;

-- Test 22: statement (line 355)
CREATE TABLE IF NOT EXISTS cpk (
  key VARCHAR(255) NOT NULL,
  value INTEGER NOT NULL,
  extra INTEGER NOT NULL,
  PRIMARY KEY (key, value)
);

-- Test 23: statement (line 363)
INSERT INTO cpk VALUES ('k1', 1, 1), ('k2', 2, 2), ('k3', 3, 3);

-- Test 24: statement (line 369)
WITH target_values (k, v) AS (
  VALUES ('k1', 1), ('k3', 3))
UPDATE cpk SET extra = (
    SELECT y+10
    FROM target_values
    INNER JOIN (VALUES (cpk.value)) v(y)
    ON TRUE
    WHERE k='k1'
)
WHERE ((cpk.key, cpk.value) IN (SELECT target_values.k, target_values.v FROM target_values));

-- Test 25: query (line 381)
SELECT * FROM cpk;

-- Test 26: statement (line 391)
CREATE TABLE t65040 (a INT, b TIMESTAMP);
INSERT INTO t65040 VALUES (1, '2001-01-01');
INSERT INTO t65040 VALUES (2, '2002-02-02');

-- Test 27: statement (line 396)
SELECT NULL
FROM t65040 AS t1
WHERE t1.b IN (
  SELECT t2.b
  FROM t65040,
    LATERAL (VALUES (t1.a)) AS v (a)
      JOIN t65040 AS t2 ON v.a = t2.a
);

-- Test 28: query (line 408)
SELECT
  (
    SELECT
      tab_4.col_4
    FROM
      (VALUES (1)) AS tab_1 (col_1)
      JOIN (
          VALUES
            (
              (
                SELECT
                  1
                FROM
                  (SELECT 1)
                WHERE
                  EXISTS(SELECT 1)
              )
            )
        )
          AS tab_6 (col_6) ON (tab_1.col_1) = (tab_6.col_6)
  )
FROM
  (VALUES (NULL)) AS tab_4 (col_4),
  (VALUES (NULL), (NULL)) AS tab_5 (col_5);

-- Test 29: statement (line 437)
CREATE TABLE t39433 AS SELECT true AS _bool;

-- Test 30: query (line 440)
SELECT
  (
    SELECT NULL
    FROM t39433 AS t0
    LIMIT 1
  )
FROM
  t39433 AS tab_57069;

-- Test 31: statement (line 611)
CREATE TABLE t89601 (i INTEGER);
INSERT INTO t89601 VALUES (0);

-- Test 32: statement (line 617)
SELECT NULL
FROM t89601 t1, t89601 t2
WHERE EXISTS(
  SELECT NULL
  FROM t89601 t3, t89601 t4
  WHERE t3.i IN (
     WITH w AS (SELECT NULL)
     SELECT t4.i::BIGINT FROM w
  )
);

-- Test 33: statement (line 631)
CREATE TABLE t1 (said, smid) AS VALUES (1, 1);
CREATE TABLE t2 (aid, said, mid, pid) AS VALUES (1, 1, 1, 1);
CREATE TABLE t3 (mid, smid) AS VALUES (1, 1);
CREATE TABLE t4 (pid INT PRIMARY KEY);
INSERT INTO t4 VALUES (1);
CREATE TABLE t5 (said, smid) AS VALUES (1, 1);

-- Test 34: statement (line 638)
DROP MATERIALIZED VIEW IF EXISTS v1;
CREATE MATERIALIZED VIEW v1 AS
    WITH
        cte1
            AS (
                SELECT
                    aid, t4.pid
                FROM
                    t1
                    INNER JOIN t2 ON t2.said = t1.said AND t2.mid = (SELECT mid FROM t3 WHERE smid = t1.smid)
                    INNER JOIN t4 ON t4.pid = t2.pid
                    INNER JOIN t3 ON t3.smid = t1.smid
            ),
        cte2
            AS (
                SELECT
                    aid, t4.pid
                FROM
                    t5
                    INNER JOIN t2 ON t2.said = t5.said AND t2.mid = (SELECT mid FROM t3 WHERE smid = t5.smid)
                    INNER JOIN t4 ON t4.pid = t2.pid
                    INNER JOIN t3 ON t3.smid = t5.smid
            )
    SELECT
        aid, pid
    FROM
        (
            SELECT aid, pid FROM cte1
            UNION
            SELECT aid, pid FROM cte2
        );

-- Test 35: statement (line 671)
DROP TYPE IF EXISTS greeting;
CREATE TYPE greeting AS ENUM ('hello', 'howdy', 'hi', 'good day', 'morning');

-- Test 36: statement (line 697)
INSERT INTO seed DEFAULT VALUES;

-- Test 37: statement (line 700)
CREATE INDEX on seed (_int8, _float8, _date);

-- Test 38: statement (line 703)
CREATE INDEX ON seed USING GIN (_jsonb);

-- Test 39: statement (line 743)
-- SET testing_optimizer_disable_rule_probability = 1.0;

-- Test 40: query (line 746)
WITH foo (bar) AS MATERIALIZED (SELECT 100)
SELECT * FROM t INNER JOIN LATERAL (
  SELECT * FROM foo WHERE bar = k*100
) ON TRUE;

-- Test 41: query (line 754)
WITH foo AS MATERIALIZED (SELECT 100)
SELECT * FROM t INNER JOIN LATERAL (
  SELECT *, (SELECT * FROM foo) FROM u WHERE l = k
) ON TRUE;

-- Test 42: query (line 767)
WITH foo AS MATERIALIZED (SELECT 100)
SELECT *, (SELECT max(m) FROM v)
FROM t INNER JOIN LATERAL (
  SELECT *, (SELECT * FROM foo), (SELECT min(m) FROM v) FROM u WHERE l = k
) ON TRUE;

-- Test 43: query (line 781)
WITH foo AS MATERIALIZED (SELECT 100)
SELECT *, (SELECT max(m) FROM v)
FROM t INNER JOIN LATERAL (
  SELECT *, (SELECT * FROM foo), (SELECT min(m) + k FROM v) FROM u WHERE l = k
) ON TRUE;

-- Test 44: query (line 795)
WITH foo AS MATERIALIZED (SELECT 100)
SELECT * FROM t INNER JOIN LATERAL (
  WITH bar AS (SELECT *, (SELECT * FROM foo) FROM u WHERE l = k) SELECT * FROM bar, foo
) ON TRUE;

-- Test 45: statement (line 807)
-- RESET testing_optimizer_disable_rule_probability;

RESET client_min_messages;
