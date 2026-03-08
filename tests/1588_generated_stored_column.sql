-- Stored Generated Column infrastructure tests.
DROP TABLE IF EXISTS gen_test_basic;
DROP TABLE IF EXISTS gen_test_alter;

-- Basic: expression referencing another column.
CREATE TABLE gen_test_basic (
    id SERIAL PRIMARY KEY,
    a INT,
    b INT GENERATED ALWAYS AS (a * 2) STORED
);

-- INSERT should compute generated column automatically.
INSERT INTO gen_test_basic (a) VALUES (5);
INSERT INTO gen_test_basic (a) VALUES (10);
SELECT 'insert_basic=' || a::text || ',' || b::text AS probe
FROM gen_test_basic ORDER BY a;

-- UPDATE source column -> generated column should recompute.
UPDATE gen_test_basic SET a = 20 WHERE a = 5;
SELECT 'update_basic=' || a::text || ',' || b::text AS probe
FROM gen_test_basic WHERE a = 20;

-- INSERT with DEFAULT for generated column should be accepted.
INSERT INTO gen_test_basic (a, b) VALUES (30, DEFAULT);
SELECT 'insert_default=' || a::text || ',' || b::text AS probe
FROM gen_test_basic WHERE a = 30;

-- UPDATE SET gen_col = DEFAULT should be accepted.
UPDATE gen_test_basic SET b = DEFAULT WHERE a = 10;
SELECT 'update_default=' || a::text || ',' || b::text AS probe
FROM gen_test_basic WHERE a = 10;

-- Reject INSERT with explicit value for generated column.
INSERT INTO gen_test_basic (a, b) VALUES (1, 2);

-- Reject UPDATE of generated column with non-DEFAULT.
UPDATE gen_test_basic SET b = 100 WHERE a = 10;

-- information_schema.columns should show generation_expression.
SELECT 'gen_expr=' || COALESCE(generation_expression, 'NULL') AS probe
FROM information_schema.columns
WHERE table_name = 'gen_test_basic' AND column_name = 'b';

SELECT 'is_generated=' || is_generated AS probe
FROM information_schema.columns
WHERE table_name = 'gen_test_basic' AND column_name = 'b';

-- Non-generated column should show NEVER / NULL.
SELECT 'is_generated_a=' || is_generated AS probe
FROM information_schema.columns
WHERE table_name = 'gen_test_basic' AND column_name = 'a';

-- ALTER TABLE ADD COLUMN with generated expression on non-empty table.
CREATE TABLE gen_test_alter (
    id SERIAL PRIMARY KEY,
    x INT
);
INSERT INTO gen_test_alter (x) VALUES (3);
INSERT INTO gen_test_alter (x) VALUES (7);
ALTER TABLE gen_test_alter ADD COLUMN y INT GENERATED ALWAYS AS (x * 10) STORED;
SELECT 'alter_backfill=' || x::text || ',' || y::text AS probe
FROM gen_test_alter ORDER BY x;
DROP TABLE gen_test_alter;

DROP TABLE gen_test_basic;
