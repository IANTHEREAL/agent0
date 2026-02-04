-- Ported from pg_tests PR#58: compatible/bytes.sql

SET client_min_messages = warning;

SELECT decode('5c78', 'hex')::bytea AS decoded;

-- bytea[] input and equality.
DROP TABLE IF EXISTS t_pgtests_bytes_array;
CREATE TABLE t_pgtests_bytes_array (col bytea[]);
INSERT INTO t_pgtests_bytes_array VALUES
  (ARRAY['a'::bytea]),
  (ARRAY['b'::bytea, 'c'::bytea]);

SELECT COUNT(*) AS eq_count
FROM t_pgtests_bytes_array
WHERE col = ARRAY['a'::bytea];

SELECT array_length(col, 1) AS len,
       encode(col[1], 'hex') AS first_hex,
       CASE WHEN array_length(col, 1) > 1 THEN encode(col[2], 'hex') ELSE NULL END AS second_hex
FROM t_pgtests_bytes_array
ORDER BY len;

DROP TABLE IF EXISTS t_pgtests_bytes_array;
