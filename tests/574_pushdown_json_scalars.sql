-- DB9_DIVERGENCE(#2402): DB9 Cop pushdown is a db9-specific exact-pair contract.
-- DB9 cop pushdown: JSON / JSONB scalar and operator surfaces currently stay
-- local on this exact pair, while on/off results keep parity.

DROP TABLE IF EXISTS db9_cop_json_scalar_smoke;
DROP TABLE IF EXISTS db9_cop_json_scalar_local_on;
DROP TABLE IF EXISTS db9_cop_json_scalar_local_off;
DROP TABLE IF EXISTS db9_cop_json_extended_local_on;
DROP TABLE IF EXISTS db9_cop_json_extended_local_off;

CREATE TABLE db9_cop_json_scalar_smoke(
    id INT PRIMARY KEY,
    n INT NOT NULL,
    payload_json JSON NOT NULL,
    payload_jsonb JSONB NOT NULL
);
CREATE INDEX db9_cop_json_scalar_smoke_n_idx ON db9_cop_json_scalar_smoke(n);
INSERT INTO db9_cop_json_scalar_smoke VALUES
    (1, 10, '{"b":2,"a":{"y":2,"x":1}}', '{"b":2,"a":{"y":2,"x":1}}'),
    (2, 20, '{"b":2,"a":{"y":2,"x":1}}', '{"b":2,"a":{"y":2,"x":1}}'),
    (3, 30, '{"b":2,"a":{"y":2,"x":1}}', '{"b":2,"a":{"y":2,"x":1}}');

SET db9.enable_cop_pushdown = on;
\o /tmp/574_json_scalar_explain.txt
EXPLAIN SELECT
    JSON_TYPEOF(payload_json) AS json_kind,
    JSONB_TYPEOF(payload_jsonb) AS jsonb_kind,
    JSON_EXTRACT_PATH_TEXT(payload_json, 'b') AS json_b_text,
    JSONB_EXTRACT_PATH_TEXT(payload_jsonb, 'b') AS jsonb_b_text,
    payload_jsonb ? 'a' AS exists_flag
FROM db9_cop_json_scalar_smoke
WHERE n = 20
LIMIT 1;
\o /tmp/574_json_scalar_local_explain.txt
EXPLAIN SELECT
    payload_jsonb #>> '{a,x}' AS hash_text,
    payload_jsonb @> '{"a":{"x":1}}'::jsonb AS contains_flag,
    payload_jsonb ?| ARRAY['z','a'] AS exists_any_flag,
    payload_jsonb ?& ARRAY['a'] AS exists_all_flag
FROM db9_cop_json_scalar_smoke
WHERE n = 20
LIMIT 1;
\o
\! if ! grep -Fq "DB9 Cop Access:" /tmp/574_json_scalar_explain.txt && ! grep -Fq "DB9 Cop Output:" /tmp/574_json_scalar_explain.txt; then echo "json_scalar_projection_stays_local|1"; else echo "json_scalar_projection_stays_local|0"; fi
\! if ! grep -Fq "DB9 Cop Access:" /tmp/574_json_scalar_local_explain.txt && ! grep -Fq "DB9 Cop Output:" /tmp/574_json_scalar_local_explain.txt; then echo "json_extended_projection_stays_local|1"; else echo "json_extended_projection_stays_local|0"; fi
SELECT
    JSON_TYPEOF(payload_json) AS json_kind,
    JSONB_TYPEOF(payload_jsonb) AS jsonb_kind,
    JSON_EXTRACT_PATH_TEXT(payload_json, 'b') AS json_b_text,
    JSONB_EXTRACT_PATH_TEXT(payload_jsonb, 'b') AS jsonb_b_text,
    payload_jsonb ? 'a' AS exists_flag
FROM db9_cop_json_scalar_smoke
WHERE n = 20
LIMIT 1;
CREATE TEMP TABLE db9_cop_json_scalar_local_on AS
SELECT
    JSON_TYPEOF(payload_json) AS json_kind,
    JSONB_TYPEOF(payload_jsonb) AS jsonb_kind,
    JSON_EXTRACT_PATH_TEXT(payload_json, 'b') AS json_b_text,
    JSONB_EXTRACT_PATH_TEXT(payload_jsonb, 'b') AS jsonb_b_text,
    payload_jsonb ? 'a' AS exists_flag
FROM db9_cop_json_scalar_smoke
WHERE n = 20
LIMIT 1;
SELECT
    payload_jsonb #>> '{a,x}' AS hash_text,
    payload_jsonb @> '{"a":{"x":1}}'::jsonb AS contains_flag,
    payload_jsonb ?| ARRAY['z','a'] AS exists_any_flag,
    payload_jsonb ?& ARRAY['a'] AS exists_all_flag
FROM db9_cop_json_scalar_smoke
WHERE n = 20
LIMIT 1;
CREATE TEMP TABLE db9_cop_json_extended_local_on AS
SELECT
    payload_jsonb #>> '{a,x}' AS hash_text,
    payload_jsonb @> '{"a":{"x":1}}'::jsonb AS contains_flag,
    payload_jsonb ?| ARRAY['z','a'] AS exists_any_flag,
    payload_jsonb ?& ARRAY['a'] AS exists_all_flag
FROM db9_cop_json_scalar_smoke
WHERE n = 20
LIMIT 1;

SET db9.enable_cop_pushdown = off;
CREATE TEMP TABLE db9_cop_json_scalar_local_off AS
SELECT
    JSON_TYPEOF(payload_json) AS json_kind,
    JSONB_TYPEOF(payload_jsonb) AS jsonb_kind,
    JSON_EXTRACT_PATH_TEXT(payload_json, 'b') AS json_b_text,
    JSONB_EXTRACT_PATH_TEXT(payload_jsonb, 'b') AS jsonb_b_text,
    payload_jsonb ? 'a' AS exists_flag
FROM db9_cop_json_scalar_smoke
WHERE n = 20
LIMIT 1;
CREATE TEMP TABLE db9_cop_json_extended_local_off AS
SELECT
    payload_jsonb #>> '{a,x}' AS hash_text,
    payload_jsonb @> '{"a":{"x":1}}'::jsonb AS contains_flag,
    payload_jsonb ?| ARRAY['z','a'] AS exists_any_flag,
    payload_jsonb ?& ARRAY['a'] AS exists_all_flag
FROM db9_cop_json_scalar_smoke
WHERE n = 20
LIMIT 1;

SELECT 'json_scalar_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_json_scalar_local_on
            EXCEPT ALL
            SELECT * FROM db9_cop_json_scalar_local_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_json_scalar_local_off
            EXCEPT ALL
            SELECT * FROM db9_cop_json_scalar_local_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

SELECT 'json_extended_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_json_extended_local_on
            EXCEPT ALL
            SELECT * FROM db9_cop_json_extended_local_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_json_extended_local_off
            EXCEPT ALL
            SELECT * FROM db9_cop_json_extended_local_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

DROP TABLE db9_cop_json_scalar_smoke;
DROP TABLE db9_cop_json_scalar_local_on;
DROP TABLE db9_cop_json_scalar_local_off;
DROP TABLE db9_cop_json_extended_local_on;
DROP TABLE db9_cop_json_extended_local_off;
