-- Issue #1950: quoted UDT identifiers must remain case-sensitive across
-- DDL default validation and ALTER COLUMN ... USING analysis.

DROP TABLE IF EXISTS qg1950_create_default_ok CASCADE;
DROP TABLE IF EXISTS qg1950_create_default_bad CASCADE;
DROP TABLE IF EXISTS qg1950_add_default_ok CASCADE;
DROP TABLE IF EXISTS qg1950_add_default_bad CASCADE;
DROP TABLE IF EXISTS qg1950_invalid_default_add CASCADE;
DROP TABLE IF EXISTS qg1950_set_default_ok CASCADE;
DROP TABLE IF EXISTS qg1950_invalid_default_set CASCADE;
DROP TABLE IF EXISTS qg1950_using_ok CASCADE;
DROP TABLE IF EXISTS qg1950_using_bad CASCADE;
DROP TABLE IF EXISTS qg1950_using_invalid_literal CASCADE;
DROP TABLE IF EXISTS qg1950_using_empty CASCADE;
DROP TABLE IF EXISTS qg1950_using_invalid_row CASCADE;
DROP TABLE IF EXISTS qg1950_invalid_default_create CASCADE;
DROP TYPE IF EXISTS mood CASCADE;

CREATE TYPE mood AS ENUM ('happy', 'sad');

CREATE TABLE qg1950_create_default_ok (c mood DEFAULT 'happy'::mood);
INSERT INTO qg1950_create_default_ok DEFAULT VALUES;
SELECT 'create_default_insert' AS check_name, c::text FROM qg1950_create_default_ok;

CREATE TABLE qg1950_nested_default_ok (
    c mood DEFAULT coalesce('happy'::mood, 'sad'::mood)
);
INSERT INTO qg1950_nested_default_ok DEFAULT VALUES;
SELECT 'nested_default_insert' AS check_name, c::text FROM qg1950_nested_default_ok;

CREATE TABLE qg1950_add_default_ok (id int);
ALTER TABLE qg1950_add_default_ok ADD COLUMN c mood DEFAULT 'happy'::mood;
INSERT INTO qg1950_add_default_ok (id) VALUES (1);
SELECT 'add_default_insert' AS check_name, id, c::text FROM qg1950_add_default_ok;

CREATE TABLE qg1950_set_default_ok (c mood);
ALTER TABLE qg1950_set_default_ok ALTER COLUMN c SET DEFAULT 'happy'::mood;
INSERT INTO qg1950_set_default_ok DEFAULT VALUES;
SELECT 'set_default_insert' AS check_name, c::text FROM qg1950_set_default_ok;

CREATE TABLE qg1950_invalid_default_add (id int);
ALTER TABLE qg1950_invalid_default_add ADD COLUMN c mood DEFAULT 'bogus'::mood;

CREATE TABLE qg1950_invalid_default_set (c mood);
ALTER TABLE qg1950_invalid_default_set ALTER COLUMN c SET DEFAULT 'bogus'::mood;

CREATE TABLE qg1950_using_ok (c text);
INSERT INTO qg1950_using_ok VALUES ('happy');
ALTER TABLE qg1950_using_ok ALTER COLUMN c TYPE mood USING c::mood;
SELECT 'alter_using_value' AS check_name, c::text FROM qg1950_using_ok;
SELECT 'alter_using_type' AS check_name, udt_name
FROM information_schema.columns
WHERE table_schema = 'public' AND table_name = 'qg1950_using_ok' AND column_name = 'c';

CREATE TABLE qg1950_using_invalid_literal (c text);
INSERT INTO qg1950_using_invalid_literal VALUES ('happy');
ALTER TABLE qg1950_using_invalid_literal ALTER COLUMN c TYPE mood USING 'bogus'::mood;

-- Empty table: enum validation must still reject invalid static casts upfront
CREATE TABLE qg1950_using_empty (c text);
ALTER TABLE qg1950_using_empty ALTER COLUMN c TYPE mood USING 'bogus'::mood;

CREATE TABLE qg1950_using_invalid_row (c text);
INSERT INTO qg1950_using_invalid_row VALUES ('bogus');
ALTER TABLE qg1950_using_invalid_row ALTER COLUMN c TYPE mood USING c::mood;

CREATE TABLE qg1950_create_default_bad (c mood DEFAULT 'happy'::"MOOD");
CREATE TABLE qg1950_invalid_default_create (c mood DEFAULT 'bogus'::mood);
CREATE TABLE qg1950_add_default_bad (id int);
ALTER TABLE qg1950_add_default_bad ADD COLUMN c mood DEFAULT 'happy'::"MOOD";
ALTER TABLE qg1950_set_default_ok ALTER COLUMN c SET DEFAULT 'happy'::"MOOD";
CREATE TABLE qg1950_using_bad (c text);
INSERT INTO qg1950_using_bad VALUES ('happy');
ALTER TABLE qg1950_using_bad ALTER COLUMN c TYPE mood USING c::"MOOD";

DROP TABLE IF EXISTS qg1950_create_default_ok CASCADE;
DROP TABLE IF EXISTS qg1950_create_default_bad CASCADE;
DROP TABLE IF EXISTS qg1950_nested_default_ok CASCADE;
DROP TABLE IF EXISTS qg1950_add_default_ok CASCADE;
DROP TABLE IF EXISTS qg1950_add_default_bad CASCADE;
DROP TABLE IF EXISTS qg1950_invalid_default_add CASCADE;
DROP TABLE IF EXISTS qg1950_set_default_ok CASCADE;
DROP TABLE IF EXISTS qg1950_invalid_default_set CASCADE;
DROP TABLE IF EXISTS qg1950_using_ok CASCADE;
DROP TABLE IF EXISTS qg1950_using_bad CASCADE;
DROP TABLE IF EXISTS qg1950_using_invalid_literal CASCADE;
DROP TABLE IF EXISTS qg1950_using_empty CASCADE;
DROP TABLE IF EXISTS qg1950_using_invalid_row CASCADE;
DROP TABLE IF EXISTS qg1950_invalid_default_create CASCADE;
DROP TYPE IF EXISTS mood CASCADE;
