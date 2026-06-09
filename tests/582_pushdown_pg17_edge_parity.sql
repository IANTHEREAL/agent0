-- DB9_DIVERGENCE(#2402): DB9 Cop pushdown is a db9-specific exact-pair contract.
-- DB9 cop pushdown: PG17 edge semantics stay identical with pushdown on/off.

DROP TABLE IF EXISTS db9_cop_pg17_edge_parity;
DROP TABLE IF EXISTS db9_cop_pg17_edge_on;
DROP TABLE IF EXISTS db9_cop_pg17_edge_off;

CREATE TABLE db9_cop_pg17_edge_parity(
    id INT PRIMARY KEY,
    n INT NOT NULL,
    hex_space TEXT NOT NULL,
    hex_newline TEXT NOT NULL,
    i4 INT NOT NULL,
    i8 BIGINT NOT NULL
);
CREATE INDEX db9_cop_pg17_edge_parity_n_idx ON db9_cop_pg17_edge_parity(n);

INSERT INTO db9_cop_pg17_edge_parity VALUES
    (1, 20, 'AB CD', E'AB\nCD', 1, 1);

SET db9.enable_cop_pushdown = on;
CREATE TEMP TABLE db9_cop_pg17_edge_on AS
SELECT
    encode(decode(hex_space, 'hex'), 'hex') AS hex_space_out,
    encode(decode(hex_newline, 'hex'), 'hex') AS hex_newline_out,
    i4 << 33 AS i4_shl_33,
    i4 << 32 AS i4_shl_32,
    i4 << -1 AS i4_shl_neg1,
    i8 >> 64 AS i8_shr_64,
    width_bucket('Infinity'::float8, 0::float8, 10::float8, 4) AS wb_inf,
    width_bucket('-Infinity'::float8, 0::float8, 10::float8, 4) AS wb_neg_inf,
    asin('NaN'::float8)::text AS asin_nan
FROM db9_cop_pg17_edge_parity
WHERE n = 20
LIMIT 1;

SET db9.enable_cop_pushdown = off;
CREATE TEMP TABLE db9_cop_pg17_edge_off AS
SELECT
    encode(decode(hex_space, 'hex'), 'hex') AS hex_space_out,
    encode(decode(hex_newline, 'hex'), 'hex') AS hex_newline_out,
    i4 << 33 AS i4_shl_33,
    i4 << 32 AS i4_shl_32,
    i4 << -1 AS i4_shl_neg1,
    i8 >> 64 AS i8_shr_64,
    width_bucket('Infinity'::float8, 0::float8, 10::float8, 4) AS wb_inf,
    width_bucket('-Infinity'::float8, 0::float8, 10::float8, 4) AS wb_neg_inf,
    asin('NaN'::float8)::text AS asin_nan
FROM db9_cop_pg17_edge_parity
WHERE n = 20
LIMIT 1;

SELECT 'pg17_edge_values' AS check_name, *
FROM db9_cop_pg17_edge_on;

SELECT 'pg17_edge_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_pg17_edge_on
            EXCEPT ALL
            SELECT * FROM db9_cop_pg17_edge_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_pg17_edge_off
            EXCEPT ALL
            SELECT * FROM db9_cop_pg17_edge_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

SET db9.enable_cop_pushdown = on;
SELECT asin('Infinity'::float8) FROM db9_cop_pg17_edge_parity WHERE n = 20 LIMIT 1;
SELECT acos('-Infinity'::float8) FROM db9_cop_pg17_edge_parity WHERE n = 20 LIMIT 1;
SELECT width_bucket('NaN'::float8, 0::float8, 10::float8, 4)
FROM db9_cop_pg17_edge_parity WHERE n = 20 LIMIT 1;
SELECT width_bucket(5::float8, '-Infinity'::float8, 10::float8, 4)
FROM db9_cop_pg17_edge_parity WHERE n = 20 LIMIT 1;
SELECT power(0::float8, -1::float8) FROM db9_cop_pg17_edge_parity WHERE n = 20 LIMIT 1;

SET db9.enable_cop_pushdown = off;
SELECT asin('Infinity'::float8) FROM db9_cop_pg17_edge_parity WHERE n = 20 LIMIT 1;
SELECT acos('-Infinity'::float8) FROM db9_cop_pg17_edge_parity WHERE n = 20 LIMIT 1;
SELECT width_bucket('NaN'::float8, 0::float8, 10::float8, 4)
FROM db9_cop_pg17_edge_parity WHERE n = 20 LIMIT 1;
SELECT width_bucket(5::float8, '-Infinity'::float8, 10::float8, 4)
FROM db9_cop_pg17_edge_parity WHERE n = 20 LIMIT 1;
SELECT power(0::float8, -1::float8) FROM db9_cop_pg17_edge_parity WHERE n = 20 LIMIT 1;

DROP TABLE db9_cop_pg17_edge_parity;
DROP TABLE db9_cop_pg17_edge_on;
DROP TABLE db9_cop_pg17_edge_off;
