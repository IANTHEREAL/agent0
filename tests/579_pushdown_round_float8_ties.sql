-- DB9_DIVERGENCE(#2402): DB9 Cop pushdown is a db9-specific exact-pair contract.
-- DB9 cop pushdown: ROUND(float8) matches PostgreSQL tie-to-even semantics.

DROP TABLE IF EXISTS db9_cop_round_float8_ties;
DROP TABLE IF EXISTS db9_cop_round_float8_ties_on;
DROP TABLE IF EXISTS db9_cop_round_float8_ties_off;

CREATE TABLE db9_cop_round_float8_ties(
    id INT PRIMARY KEY,
    n INT NOT NULL,
    f8 DOUBLE PRECISION NOT NULL
);
CREATE INDEX db9_cop_round_float8_ties_n_idx ON db9_cop_round_float8_ties(n);

INSERT INTO db9_cop_round_float8_ties VALUES
    (1, 10, 2.5),
    (2, 20, 3.5),
    (3, 30, -2.5),
    (4, 40, -3.5);

SET db9.enable_cop_pushdown = on;
\o /tmp/579_round_float8_ties_explain.txt
EXPLAIN SELECT
    ROUND(f8) AS rounded
FROM db9_cop_round_float8_ties
WHERE n = 20
LIMIT 1;
\o
\! if grep -Fq "DB9 Cop Access: point (20)" /tmp/579_round_float8_ties_explain.txt && grep -Fq "DB9 Cop Output: rounded" /tmp/579_round_float8_ties_explain.txt; then echo "round_float8_ties_projection_folds|1"; else echo "round_float8_ties_projection_folds|0"; fi

CREATE TEMP TABLE db9_cop_round_float8_ties_on AS
SELECT
    id,
    ROUND(f8) AS rounded
FROM db9_cop_round_float8_ties
ORDER BY id;

SET db9.enable_cop_pushdown = off;
CREATE TEMP TABLE db9_cop_round_float8_ties_off AS
SELECT
    id,
    ROUND(f8) AS rounded
FROM db9_cop_round_float8_ties
ORDER BY id;

SELECT 'round_float8_ties_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_round_float8_ties_on
            EXCEPT ALL
            SELECT * FROM db9_cop_round_float8_ties_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_round_float8_ties_off
            EXCEPT ALL
            SELECT * FROM db9_cop_round_float8_ties_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

SELECT 'round_float8_tie_value' AS check_name, id, rounded
FROM db9_cop_round_float8_ties_on
ORDER BY id;

\! rm -f /tmp/579_round_float8_ties_explain.txt

DROP TABLE db9_cop_round_float8_ties;
DROP TABLE db9_cop_round_float8_ties_on;
DROP TABLE db9_cop_round_float8_ties_off;
