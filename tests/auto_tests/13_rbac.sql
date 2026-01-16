-- Auto tests: RBAC (roles and grants)
CREATE SCHEMA IF NOT EXISTS auto_tests;
SET search_path TO auto_tests, public;

DROP TABLE IF EXISTS rbac_items;

CREATE TABLE rbac_items (
    id INT PRIMARY KEY,
    val TEXT
);

INSERT INTO rbac_items (id, val) VALUES (1, 'x');

DROP ROLE IF EXISTS auto_tests_reader;
CREATE ROLE auto_tests_reader LOGIN PASSWORD 'readpass';

GRANT SELECT ON rbac_items TO auto_tests_reader;
REVOKE SELECT ON rbac_items FROM auto_tests_reader;

DROP ROLE auto_tests_reader;
DROP TABLE rbac_items;
