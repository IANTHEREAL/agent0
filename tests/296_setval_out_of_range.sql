-- #1582: setval() out-of-range errors must include sequence bounds (min..max).

DROP TABLE IF EXISTS t;
DROP SEQUENCE IF EXISTS setval_custom_bounds_seq;

CREATE TABLE t (id BIGSERIAL PRIMARY KEY);

-- BIGSERIAL-backed implicit sequence lower bound violation.
SELECT setval('t_id_seq', 0);

-- Explicit sequence upper bound violation.
CREATE SEQUENCE setval_custom_bounds_seq MINVALUE 1 MAXVALUE 10;
SELECT setval('setval_custom_bounds_seq', 11);

DROP TABLE t;
DROP SEQUENCE setval_custom_bounds_seq;
