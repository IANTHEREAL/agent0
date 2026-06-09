-- DB9_DIVERGENCE(#2402): DB9 Cop pushdown is a db9-specific exact-pair contract.
-- DB9 cop pushdown: width_bucket errors and overlay edge results are consistent on/off.

DROP TABLE IF EXISTS db9_cop_width_overlay_error_smoke;

CREATE TABLE db9_cop_width_overlay_error_smoke(
    id INT PRIMARY KEY,
    n INT NOT NULL,
    txt TEXT NOT NULL,
    bin BYTEA NOT NULL
);
CREATE INDEX db9_cop_width_overlay_error_smoke_n_idx
    ON db9_cop_width_overlay_error_smoke(n);

INSERT INTO db9_cop_width_overlay_error_smoke VALUES
    (1, 10, 'abcdef', '\x010203'),
    (2, 20, 'ghijkl', '\x040506');

SET db9.enable_cop_pushdown = on;
EXPLAIN SELECT
    width_bucket(10::float8, 0::float8, 1::float8, 2147483647::int) AS wb,
    overlay(txt placing 'Z' from 2 for -1) AS overlay_text,
    overlay(bin placing '\xff'::bytea from 2 for -1) AS overlay_bytea
FROM db9_cop_width_overlay_error_smoke
WHERE n = 10
LIMIT 1;
SELECT width_bucket(10::float8, 0::float8, 1::float8, 2147483647::int) AS wb
FROM db9_cop_width_overlay_error_smoke
WHERE n = 10
LIMIT 1;
SELECT
    width_bucket('Infinity'::float8, 0::float8, 10::float8, 4) AS wb_inf_asc,
    width_bucket('-Infinity'::float8, 0::float8, 10::float8, 4) AS wb_neg_inf_asc,
    width_bucket('Infinity'::float8, 10::float8, 0::float8, 4) AS wb_inf_desc,
    width_bucket('-Infinity'::float8, 10::float8, 0::float8, 4) AS wb_neg_inf_desc
FROM db9_cop_width_overlay_error_smoke
WHERE n = 10
LIMIT 1;
SELECT width_bucket('NaN'::float8, 0::float8, 10::float8, 4) AS wb_nan_operand
FROM db9_cop_width_overlay_error_smoke
WHERE n = 10
LIMIT 1;
SELECT width_bucket(5::float8, '-Infinity'::float8, 10::float8, 4) AS wb_inf_bound
FROM db9_cop_width_overlay_error_smoke
WHERE n = 10
LIMIT 1;
SELECT overlay(txt placing 'Z' from 2 for -1) AS overlay_text
FROM db9_cop_width_overlay_error_smoke
WHERE n = 10
LIMIT 1;
SELECT overlay(bin placing '\xff'::bytea from 2 for -1) AS overlay_bytea
FROM db9_cop_width_overlay_error_smoke
WHERE n = 10
LIMIT 1;

SET db9.enable_cop_pushdown = off;
SELECT width_bucket(10::float8, 0::float8, 1::float8, 2147483647::int) AS wb
FROM db9_cop_width_overlay_error_smoke
WHERE n = 10
LIMIT 1;
SELECT
    width_bucket('Infinity'::float8, 0::float8, 10::float8, 4) AS wb_inf_asc,
    width_bucket('-Infinity'::float8, 0::float8, 10::float8, 4) AS wb_neg_inf_asc,
    width_bucket('Infinity'::float8, 10::float8, 0::float8, 4) AS wb_inf_desc,
    width_bucket('-Infinity'::float8, 10::float8, 0::float8, 4) AS wb_neg_inf_desc
FROM db9_cop_width_overlay_error_smoke
WHERE n = 10
LIMIT 1;
SELECT width_bucket('NaN'::float8, 0::float8, 10::float8, 4) AS wb_nan_operand
FROM db9_cop_width_overlay_error_smoke
WHERE n = 10
LIMIT 1;
SELECT width_bucket(5::float8, '-Infinity'::float8, 10::float8, 4) AS wb_inf_bound
FROM db9_cop_width_overlay_error_smoke
WHERE n = 10
LIMIT 1;
SELECT overlay(txt placing 'Z' from 2 for -1) AS overlay_text
FROM db9_cop_width_overlay_error_smoke
WHERE n = 10
LIMIT 1;
SELECT overlay(bin placing '\xff'::bytea from 2 for -1) AS overlay_bytea
FROM db9_cop_width_overlay_error_smoke
WHERE n = 10
LIMIT 1;

DROP TABLE db9_cop_width_overlay_error_smoke;
