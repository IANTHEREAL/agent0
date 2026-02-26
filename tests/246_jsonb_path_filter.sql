-- JSONB path filtering: #>> operator with text array path + comparison.
-- Validates Prisma-style JSONB path filter patterns.

DROP TABLE IF EXISTS t_jsonb_path CASCADE;

CREATE TABLE t_jsonb_path (
    id SERIAL PRIMARY KEY,
    name VARCHAR(100) NOT NULL,
    metadata JSONB
);

INSERT INTO t_jsonb_path (name, metadata) VALUES
    ('Alice', '{"role": "admin", "level": 5, "tags": ["dev", "lead"]}'),
    ('Bob', '{"role": "user", "level": 2, "tags": ["design"]}'),
    ('Charlie', NULL);

-- #>> with text array literal path (Prisma pattern: equals).
SELECT name FROM t_jsonb_path WHERE (metadata #>> '{role}') = 'admin' ORDER BY name;

-- #>> with text array literal path (Prisma pattern: gte with cast).
SELECT name FROM t_jsonb_path WHERE ((metadata #>> '{level}')::DECIMAL) >= 3 ORDER BY name;

-- #>> with nested path.
SELECT name, metadata #>> '{role}' AS role FROM t_jsonb_path
WHERE metadata IS NOT NULL ORDER BY name;

-- Containment operator @> still works.
SELECT name FROM t_jsonb_path WHERE metadata @> '{"role": "admin"}' ORDER BY name;

DROP TABLE t_jsonb_path CASCADE;
