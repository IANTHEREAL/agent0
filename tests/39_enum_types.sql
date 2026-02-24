-- Enum type creation and use
SET client_min_messages = warning;

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

-- ── ALTER TYPE enum tests ────────────────────────────────────────────

-- Setup fresh enum for ALTER TYPE tests
DROP TABLE IF EXISTS alter_enum_test;
DROP TYPE IF EXISTS color;

CREATE TYPE color AS ENUM ('red', 'green', 'blue');

CREATE TABLE alter_enum_test (
  id INT PRIMARY KEY,
  c color
);

INSERT INTO alter_enum_test (id, c) VALUES (1, 'red');
INSERT INTO alter_enum_test (id, c) VALUES (2, 'green');
INSERT INTO alter_enum_test (id, c) VALUES (3, 'blue');

-- ADD VALUE at end
ALTER TYPE color ADD VALUE 'yellow';
SELECT e.enumlabel, e.enumsortorder
FROM pg_catalog.pg_type t
JOIN pg_catalog.pg_enum e ON t.oid = e.enumtypid
WHERE t.typname = 'color'
ORDER BY e.enumsortorder;

-- ADD VALUE BEFORE existing label
ALTER TYPE color ADD VALUE 'cyan' BEFORE 'green';
SELECT e.enumlabel, e.enumsortorder
FROM pg_catalog.pg_type t
JOIN pg_catalog.pg_enum e ON t.oid = e.enumtypid
WHERE t.typname = 'color'
ORDER BY e.enumsortorder;

-- ADD VALUE AFTER existing label
ALTER TYPE color ADD VALUE 'magenta' AFTER 'blue';
SELECT e.enumlabel, e.enumsortorder
FROM pg_catalog.pg_type t
JOIN pg_catalog.pg_enum e ON t.oid = e.enumtypid
WHERE t.typname = 'color'
ORDER BY e.enumsortorder;

-- ADD VALUE IF NOT EXISTS (already present — no error)
ALTER TYPE color ADD VALUE IF NOT EXISTS 'red';

-- ADD VALUE duplicate without IF NOT EXISTS — error
ALTER TYPE color ADD VALUE 'red';

-- ADD VALUE with invalid BEFORE reference — error
ALTER TYPE color ADD VALUE 'orange' BEFORE 'nonexistent';

-- Insert using new enum value
INSERT INTO alter_enum_test (id, c) VALUES (4, 'yellow');
SELECT id, c FROM alter_enum_test ORDER BY id;

-- RENAME VALUE
ALTER TYPE color RENAME VALUE 'red' TO 'scarlet';
SELECT e.enumlabel, e.enumsortorder
FROM pg_catalog.pg_type t
JOIN pg_catalog.pg_enum e ON t.oid = e.enumtypid
WHERE t.typname = 'color'
ORDER BY e.enumsortorder;

-- Verify stored rows reflect renamed value
SELECT id, c FROM alter_enum_test ORDER BY id;

-- RENAME VALUE error — old label doesn't exist
ALTER TYPE color RENAME VALUE 'red' TO 'crimson';

-- RENAME VALUE error — new label already exists
ALTER TYPE color RENAME VALUE 'scarlet' TO 'green';

-- RENAME TO
ALTER TYPE color RENAME TO palette;

-- Verify type is renamed in pg_type
SELECT typname, typtype FROM pg_catalog.pg_type WHERE typname = 'palette';

-- Verify column still works after type rename
INSERT INTO alter_enum_test (id, c) VALUES (5, 'green');
SELECT id, c FROM alter_enum_test ORDER BY id;

-- Verify column data_type reference updated in information_schema
SELECT column_name, udt_name
FROM information_schema.columns
WHERE table_name = 'alter_enum_test' AND column_name = 'c';

-- Cleanup
DROP TABLE alter_enum_test;
DROP TYPE palette;

-- ── RENAME VALUE with DEFAULT expression ───────────────────────────
-- Verifies that column defaults referencing an old enum label are
-- rewritten so subsequent inserts succeed (P1 regression path).

DROP TABLE IF EXISTS enum_default_test;
DROP TYPE IF EXISTS status;

CREATE TYPE status AS ENUM ('active', 'inactive', 'banned');
CREATE TABLE enum_default_test (
  id INT PRIMARY KEY,
  s status DEFAULT 'active'
);

INSERT INTO enum_default_test (id) VALUES (1);
SELECT id, s FROM enum_default_test ORDER BY id;

-- Rename the default label
ALTER TYPE status RENAME VALUE 'active' TO 'enabled';

-- Insert using the (now-rewritten) default — must succeed
INSERT INTO enum_default_test (id) VALUES (2);
SELECT id, s FROM enum_default_test ORDER BY id;

-- Explicit insert with the new label
INSERT INTO enum_default_test (id, s) VALUES (3, 'enabled');
SELECT id, s FROM enum_default_test ORDER BY id;

DROP TABLE enum_default_test;
DROP TYPE status;

-- ── RENAME VALUE with FK-referenced enum column ───────────────────
-- The enum column is the FK-referenced column itself, so the rename
-- changes the FK-referenced values.  Without the P0 fix the FK ON
-- UPDATE NO ACTION handler would raise a spurious violation.

DROP TABLE IF EXISTS fk_child;
DROP TABLE IF EXISTS fk_parent;
DROP TYPE IF EXISTS priority;

CREATE TYPE priority AS ENUM ('low', 'medium', 'high');

CREATE TABLE fk_parent (
  id INT PRIMARY KEY,
  p priority UNIQUE NOT NULL
);
CREATE TABLE fk_child (
  id INT PRIMARY KEY,
  p priority NOT NULL REFERENCES fk_parent(p)
);

INSERT INTO fk_parent (id, p) VALUES (1, 'low'), (2, 'medium');
INSERT INTO fk_child (id, p) VALUES (10, 'low'), (20, 'medium');

-- Rename an enum value used in the FK-referenced column — must succeed
ALTER TYPE priority RENAME VALUE 'low' TO 'minimal';

SELECT id, p FROM fk_parent ORDER BY id;
SELECT id, p FROM fk_child ORDER BY id;

DROP TABLE fk_child;
DROP TABLE fk_parent;
DROP TYPE priority;

-- ── RENAME TO with cast in default expression ─────────────────────
-- After ALTER TYPE mood RENAME TO feeling, a default like
-- 'happy'::mood must have the cast reference updated to ::feeling.

DROP TABLE IF EXISTS rename_cast_test;
DROP TYPE IF EXISTS mood;

CREATE TYPE mood AS ENUM ('happy', 'sad');
CREATE TABLE rename_cast_test (
  id INT PRIMARY KEY,
  m mood DEFAULT 'happy'::mood
);

INSERT INTO rename_cast_test (id) VALUES (1);
SELECT id, m FROM rename_cast_test ORDER BY id;

ALTER TYPE mood RENAME TO feeling;

-- Insert using the default — cast reference must have been rewritten
INSERT INTO rename_cast_test (id) VALUES (2);
SELECT id, m FROM rename_cast_test ORDER BY id;

DROP TABLE rename_cast_test;
DROP TYPE feeling;

-- ── Empty enum creation test ────────────────────────────────────────

DROP TYPE IF EXISTS empty_enum;
CREATE TYPE empty_enum AS ENUM ();
SELECT e.enumlabel FROM pg_catalog.pg_type t
JOIN pg_catalog.pg_enum e ON t.oid = e.enumtypid
WHERE t.typname = 'empty_enum'
ORDER BY e.enumsortorder;
ALTER TYPE empty_enum ADD VALUE 'first';
SELECT e.enumlabel FROM pg_catalog.pg_type t
JOIN pg_catalog.pg_enum e ON t.oid = e.enumtypid
WHERE t.typname = 'empty_enum'
ORDER BY e.enumsortorder;
DROP TYPE empty_enum;

-- Composite types: create + introspect, but not usable as a column type
CREATE TYPE comp AS (a INT, b TEXT);
SELECT typname, typtype FROM pg_catalog.pg_type WHERE typname = 'comp';
CREATE TABLE udt_comp_test (id INT PRIMARY KEY, c comp);
DROP TYPE comp;
DROP TABLE IF EXISTS udt_comp_test;
DROP TYPE IF EXISTS comp;
