-- Ported from pg_tests PR#58: compatible/bpchar.sql

SET client_min_messages = warning;

-- Type name alias should work.
SELECT 'foo'::BPCHAR AS bpchar_cast;

-- Table round-trip.
DROP TABLE IF EXISTS t_pgtests_bpchar;
CREATE TABLE t_pgtests_bpchar (c BPCHAR PRIMARY KEY);

INSERT INTO t_pgtests_bpchar VALUES ('foo'), ('ba'), ('c'), ('foobarbaz');

SELECT c FROM t_pgtests_bpchar ORDER BY c;

DROP TABLE IF EXISTS t_pgtests_bpchar;
