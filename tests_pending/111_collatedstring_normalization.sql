-- PostgreSQL compatible tests from collatedstring_normalization
-- 5 tests

-- CockroachDB's "fr" collation compares canonically equivalent strings as equal.
-- In PostgreSQL, use a nondeterministic ICU collation so equality honors collation rules.
CREATE COLLATION IF NOT EXISTS public.fr (provider = icu, locale = 'fr', deterministic = false);

CREATE TABLE t (a TEXT COLLATE fr);

-- Test 1: statement (line 8)
INSERT INTO t VALUES (convert_from(decode('416d65cc816c6965', 'hex'), 'UTF8') COLLATE fr);

-- Test 2: query (line 12)
SELECT a FROM t WHERE a = (convert_from(decode('416dc3a96c6965', 'hex'), 'UTF8') COLLATE fr);

-- Test 3: statement (line 17)
DELETE FROM t;

-- Test 4: statement (line 21)
INSERT INTO t VALUES (convert_from(decode('416dc3a96c6965', 'hex'), 'UTF8') COLLATE fr);

-- Test 5: query (line 25)
SELECT a FROM t WHERE a = (convert_from(decode('416d65cc816c6965', 'hex'), 'UTF8') COLLATE fr);
