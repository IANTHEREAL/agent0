-- psql \d / \d+ compatibility regression test
--
-- Exercises the exact query patterns psql generates for \d and \d+.
-- Each section is labelled with the psql metacommand that generates it.

DROP TABLE IF EXISTS _psql_compat CASCADE;

CREATE TABLE _psql_compat (
    id SERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    score INT DEFAULT 0,
    tags TEXT[]
);

-- ============================================================
-- Q1: psql \d table lookup (OPERATOR(pg_catalog.~) + COLLATE)
-- ============================================================
SELECT 'q1_lookup=' || c.relname AS q1_lookup
FROM pg_catalog.pg_class c
     LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
WHERE c.relname OPERATOR(pg_catalog.~) '^(_psql_compat)$' COLLATE pg_catalog."default"
  AND pg_catalog.pg_table_is_visible(c.oid)
ORDER BY 1;

-- ============================================================
-- Q2: psql \d table metadata (pg_class new columns)
-- ============================================================
SELECT 'q2_relkind=' || c.relkind::text
    || ',checks=' || c.relchecks
    || ',hasindex=' || c.relhasindex::text
    || ',hasrules=' || c.relhasrules::text
    || ',hastriggers=' || c.relhastriggers::text
    || ',rowsecurity=' || c.relrowsecurity::text
    || ',forcerowsecurity=' || c.relforcerowsecurity::text
    || ',ispartition=' || c.relispartition::text
    || ',persistence=' || c.relpersistence::text
    || ',replident=' || c.relreplident::text
FROM pg_catalog.pg_class c
LEFT JOIN pg_catalog.pg_class tc ON (c.reltoastrelid = tc.oid)
WHERE c.relname = '_psql_compat'
  AND pg_catalog.pg_table_is_visible(c.oid);

-- ============================================================
-- Q3: psql \d column details (pg_attribute with attcollation)
-- ============================================================
SELECT 'q3_col=' || a.attname
    || ',notnull=' || a.attnotnull::text
    || ',collation=' || a.attcollation
    || ',identity=' || a.attidentity::text
    || ',generated=' || a.attgenerated::text
FROM pg_catalog.pg_attribute a
JOIN pg_catalog.pg_class c ON a.attrelid = c.oid
WHERE c.relname = '_psql_compat'
  AND pg_catalog.pg_table_is_visible(c.oid)
  AND a.attnum > 0 AND NOT a.attisdropped
ORDER BY a.attnum;

-- ============================================================
-- Q4: psql \d+ TOAST options (unnest as FROM table function)
-- ============================================================
SELECT 'q4_unnest=' || x
FROM pg_catalog.unnest(ARRAY['option1=val1', 'option2=val2']) x
ORDER BY 1;

-- ============================================================
-- Q5: OPERATOR(pg_catalog.->) JSON access
-- ============================================================
SELECT 'q5_json=' || (('{"k":"v"}'::jsonb OPERATOR(pg_catalog.->) 'k')::text);

-- ============================================================
-- Q6: OPERATOR(pg_catalog.->>) JSON text access
-- ============================================================
SELECT 'q6_json_text=' || ('{"k":"v"}'::jsonb OPERATOR(pg_catalog.->>) 'k');

-- ============================================================
-- Q7: Schema-qualified comparison (OPERATOR(pg_catalog.=))
-- ============================================================
SELECT 'q7_eq=' || ((1 OPERATOR(pg_catalog.=) 1)::text);

-- ============================================================
-- Q8: psql \d index details (pg_index.indisreplident)
-- ============================================================
SELECT 'q8_index=' || c2.relname || ',replident=' || i.indisreplident::text
FROM pg_catalog.pg_class c, pg_catalog.pg_class c2, pg_catalog.pg_index i
WHERE c.relname = '_psql_compat'
  AND pg_catalog.pg_table_is_visible(c.oid)
  AND c.oid = i.indrelid
  AND i.indexrelid = c2.oid
ORDER BY c2.relname;

-- Cleanup
DROP TABLE _psql_compat CASCADE;
