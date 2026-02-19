-- Enum type creation and use
DROP TABLE IF EXISTS udt_enum_test;
DROP TYPE IF EXISTS role;
DROP TABLE IF EXISTS udt_comp_test;
DROP TYPE IF EXISTS comp;

CREATE TYPE role AS ENUM ('USER', 'ADMIN');

CREATE TABLE udt_enum_test (
  id INT PRIMARY KEY,
  r role
);

INSERT INTO udt_enum_test (id, r) VALUES (1, 'USER');
INSERT INTO udt_enum_test (id, r) VALUES (2, NULL);
-- Expected error (invalid enum label)
INSERT INTO udt_enum_test (id, r) VALUES (3, 'INVALID');

-- information_schema should preserve declared type
SELECT column_name, data_type, udt_schema, udt_name
FROM information_schema.columns
WHERE table_name = 'udt_enum_test'
ORDER BY ordinal_position;

-- pg_catalog enum introspection (pg_type join pg_enum)
SELECT t.typname, t.typtype, t.typcategory, e.enumlabel, e.enumsortorder
FROM pg_catalog.pg_type t
JOIN pg_catalog.pg_enum e ON t.oid = e.enumtypid
WHERE t.typname = 'role'
ORDER BY e.enumsortorder;

-- pg_attribute should point enum column at enum type oid
SELECT a.attname, t.typname
FROM pg_catalog.pg_attribute a
JOIN pg_catalog.pg_type t ON a.atttypid = t.oid
JOIN pg_catalog.pg_class c ON a.attrelid = c.oid
WHERE c.relname = 'udt_enum_test' AND a.attname = 'r';

-- DROP TYPE should be RESTRICT (type still referenced)
DROP TYPE role;

DROP TABLE udt_enum_test;
DROP TYPE role;
DROP TYPE IF EXISTS role;

-- Composite types: create + introspect, but not usable as a column type
CREATE TYPE comp AS (a INT, b TEXT);
SELECT typname, typtype FROM pg_catalog.pg_type WHERE typname = 'comp';
CREATE TABLE udt_comp_test (id INT PRIMARY KEY, c comp);
DROP TYPE comp;
DROP TABLE IF EXISTS udt_comp_test;
DROP TYPE IF EXISTS comp;
