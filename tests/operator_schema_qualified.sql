-- Test: schema-qualified OPERATOR() syntax (psql \d compatibility)
-- psql uses OPERATOR(pg_catalog.~) in its \d introspection queries.

-- Schema-qualified regex match (the exact pattern psql generates)
SELECT 'blog_chunks' OPERATOR(pg_catalog.~) '^(blog_chunks)$' AS regex_match;
SELECT 'HELLO' OPERATOR(pg_catalog.~*) '^hello$' AS regex_imatch;
SELECT 'goodbye' OPERATOR(pg_catalog.!~) '^hello' AS regex_not_match;
SELECT 'GOODBYE' OPERATOR(pg_catalog.!~*) '^hello' AS regex_not_imatch;

-- Schema-qualified comparison operators
SELECT 1 OPERATOR(pg_catalog.=) 1 AS eq_true;
SELECT 1 OPERATOR(pg_catalog.<>) 2 AS neq_true;
SELECT 1 OPERATOR(pg_catalog.<) 2 AS lt_true;
SELECT 2 OPERATOR(pg_catalog.>) 1 AS gt_true;

-- Schema-qualified string concat
SELECT 'hello' OPERATOR(pg_catalog.||) ' world' AS concat_result;

-- Unqualified OPERATOR() syntax
SELECT 'test' OPERATOR(~) '^test$' AS unqualified_regex;
