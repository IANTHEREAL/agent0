-- Issue #2065: IS DISTINCT FROM / IS NOT DISTINCT FROM must apply implicit
-- type coercion between compatible types, matching PostgreSQL behaviour.

DROP TABLE IF EXISTS t_2065 CASCADE;
CREATE TABLE t_2065 (dept TEXT, val INT);
INSERT INTO t_2065 VALUES ('a', 1), ('a', 2), ('b', 3);

-- Core regression: count(*) returns Int64, literal 0 is Int32.
SELECT dept, count(*) FROM t_2065 GROUP BY dept HAVING count(*) IS DISTINCT FROM 0 ORDER BY dept;

-- Cross-type coercion: bigint vs int
SELECT 1::bigint IS DISTINCT FROM 1::int;
SELECT 1::bigint IS NOT DISTINCT FROM 1::int;
SELECT 1::bigint IS DISTINCT FROM 2::int;

-- NULL semantics (should still work)
SELECT NULL IS DISTINCT FROM 1;
SELECT NULL IS NOT DISTINCT FROM NULL;

-- Float vs int coercion
SELECT 1.5::float8 IS DISTINCT FROM 1::int;

-- Bare string literal (unknown) vs int: coerce allowed (PG accepts)
SELECT '1' IS DISTINCT FROM 1;

-- Explicit ::text vs int: must error (PG rejects)
SELECT '1'::text IS DISTINCT FROM 1;
SELECT '1'::text IS NOT DISTINCT FROM '1'::int;

-- Text vs text: always allowed
SELECT 'hello'::text IS DISTINCT FROM 'world'::text;

-- Parameter inference: $1 IS DISTINCT FROM literal → infer $1 as text
PREPARE p2065a AS SELECT $1 IS DISTINCT FROM 'x';
EXECUTE p2065a('hello');
EXECUTE p2065a('x');
DEALLOCATE p2065a;

-- Parameter inference: $1 IS NOT DISTINCT FROM $2 → both resolve to text
PREPARE p2065b AS SELECT $1 IS NOT DISTINCT FROM $2;
EXECUTE p2065b('a', 'a');
EXECUTE p2065b('a', 'b');
DEALLOCATE p2065b;

-- Parameter + NULL: PG rejects (grammar-level construct, no "default to text" fallback)
PREPARE p2065d AS SELECT $1 IS DISTINCT FROM NULL;
PREPARE p2065e AS SELECT $1 IS NOT DISTINCT FROM NULL;

-- Parameter inference: $1 vs varchar(3) → infer $1 as text (PG normalizes typmod)
PREPARE p2065f AS SELECT $1 IS DISTINCT FROM 'x'::varchar(3);
EXECUTE p2065f('hello');
EXECUTE p2065f('x');
DEALLOCATE p2065f;

-- Parameter inference: $1 vs name → infer $1 as name (PG keeps name)
PREPARE p2065g AS SELECT $1 IS DISTINCT FROM 'x'::name;
EXECUTE p2065g('hello');
EXECUTE p2065g('x');
DEALLOCATE p2065g;

-- Resolved param + unresolved param: PG infers $2 from $1's declared type
PREPARE p2065h(text) AS SELECT $1 IS DISTINCT FROM $2;
EXECUTE p2065h('a', 'b');
EXECUTE p2065h('a', 'a');
DEALLOCATE p2065h;

PREPARE p2065i(int) AS SELECT $1 IS DISTINCT FROM $2;
EXECUTE p2065i(1, 2);
EXECUTE p2065i(1, 1);
DEALLOCATE p2065i;

-- JSON: PG has no = operator for json; unknown literal/param → json must error
SELECT '{}' IS DISTINCT FROM '{}'::json;
PREPARE p2065j AS SELECT $1 IS DISTINCT FROM '{}'::json;

-- BUT: NULL IS DISTINCT FROM json is always valid (NULL short-circuits)
SELECT NULL IS DISTINCT FROM '{}'::json;
SELECT NULL IS NOT DISTINCT FROM '{}'::json;
SELECT '{}'::json IS DISTINCT FROM NULL;

-- Same-type json = json: PG rejects (no = operator)
SELECT '{}'::json IS DISTINCT FROM '{}'::json;
SELECT '{}'::json IS NOT DISTINCT FROM '{}'::json;

-- Same-type jsonb = jsonb: PG accepts
SELECT '{}'::jsonb IS DISTINCT FROM '{}'::jsonb;
SELECT '{}'::jsonb IS NOT DISTINCT FROM '{}'::jsonb;

-- JSONB: PG accepts unknown → jsonb
SELECT '{}' IS DISTINCT FROM '{}'::jsonb;
PREPARE p2065k AS SELECT $1 IS DISTINCT FROM '{}'::jsonb;
EXECUTE p2065k('{"a":1}');
EXECUTE p2065k('{}');
DEALLOCATE p2065k;

-- OID alias: PG infers unresolved param as oid (Int64), not the alias type
PREPARE p2065l(regclass) AS SELECT $1 IS DISTINCT FROM $2;
EXECUTE p2065l('t_2065', 0);
DEALLOCATE p2065l;

PREPARE p2065m(regtype) AS SELECT $1 IS DISTINCT FROM $2;
EXECUTE p2065m('int4', 23);
EXECUTE p2065m('int4', 0);
DEALLOCATE p2065m;

-- Parameter inference with explicit typed side: int vs bigint coercion
PREPARE p2065c(int) AS SELECT $1 IS DISTINCT FROM 0::bigint;
EXECUTE p2065c(0);
EXECUTE p2065c(1);
DEALLOCATE p2065c;

-- Incompatible cross-type pairs: PG rejects at analysis time
SELECT 1 IS DISTINCT FROM true;
SELECT '{}'::jsonb IS DISTINCT FROM '{}'::json;

DROP TABLE t_2065;
