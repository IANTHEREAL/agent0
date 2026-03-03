-- JSONB ORDER BY: verify sort ordering matches PostgreSQL semantics.
-- Tests pre-parsed JSONB sort key optimization (issue #1264).

DROP TABLE IF EXISTS t_jsonb_sort CASCADE;

CREATE TABLE t_jsonb_sort (
    id SERIAL PRIMARY KEY,
    data JSONB
);

-- Mixed JSONB types: PG order is Null < String < Number < Boolean < Array < Object
INSERT INTO t_jsonb_sort (data) VALUES
    ('{"a":1}'),
    ('[1,2,3]'),
    ('true'),
    ('42'),
    ('"hello"'),
    ('null');

SELECT data FROM t_jsonb_sort ORDER BY data;

-- SQL NULLs mixed with JSONB data
DROP TABLE IF EXISTS t_jsonb_nulls CASCADE;

CREATE TABLE t_jsonb_nulls (
    id SERIAL PRIMARY KEY,
    val JSONB
);

INSERT INTO t_jsonb_nulls (val) VALUES
    ('1'),
    (NULL),
    ('3'),
    (NULL),
    ('2');

-- ASC NULLS LAST (default for ASC)
SELECT id, val FROM t_jsonb_nulls ORDER BY val ASC NULLS LAST, id;

-- ASC NULLS FIRST
SELECT id, val FROM t_jsonb_nulls ORDER BY val ASC NULLS FIRST, id;

-- DESC NULLS FIRST (default for DESC)
SELECT id, val FROM t_jsonb_nulls ORDER BY val DESC NULLS FIRST, id;

-- DESC NULLS LAST
SELECT id, val FROM t_jsonb_nulls ORDER BY val DESC NULLS LAST, id;

-- Multi-column ORDER BY with JSONB + non-JSONB
DROP TABLE IF EXISTS t_jsonb_multi CASCADE;

CREATE TABLE t_jsonb_multi (
    id SERIAL PRIMARY KEY,
    grp INT NOT NULL,
    payload JSONB
);

INSERT INTO t_jsonb_multi (grp, payload) VALUES
    (1, '10'),
    (1, '2'),
    (2, '30'),
    (2, '1');

SELECT grp, payload FROM t_jsonb_multi ORDER BY grp, payload;

-- Nested objects/arrays ordering
SELECT data FROM t_jsonb_sort ORDER BY data DESC;

DROP TABLE t_jsonb_sort CASCADE;
DROP TABLE t_jsonb_nulls CASCADE;
DROP TABLE t_jsonb_multi CASCADE;
