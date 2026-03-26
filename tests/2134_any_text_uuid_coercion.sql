-- Issue #2134: ANY() with text array vs UUID column requires implicit coercion.

DROP TABLE IF EXISTS t2134;
CREATE TABLE t2134 (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name TEXT
);

INSERT INTO t2134 (id, name) VALUES
    ('10e239f0-f965-4819-a672-b912072c73f1', 'alice'),
    ('114da388-bf39-4f59-b8ba-ee88f8c49521', 'bob'),
    ('550e8400-e29b-41d4-a716-446655440000', 'carol');

-- 1) UUID column = ANY(uuid_literal_array) — basic case.
SELECT name FROM t2134
WHERE id = ANY(ARRAY['10e239f0-f965-4819-a672-b912072c73f1'::uuid, '114da388-bf39-4f59-b8ba-ee88f8c49521'::uuid])
ORDER BY name;

-- 2) UUID column with explicit ::uuid[] cast on text literal array.
SELECT name FROM t2134
WHERE id = ANY(ARRAY['10e239f0-f965-4819-a672-b912072c73f1', '114da388-bf39-4f59-b8ba-ee88f8c49521']::uuid[])
ORDER BY name;

-- 3) Single UUID comparison: uuid_col = text_literal (implicit coercion).
SELECT name FROM t2134
WHERE id = '550e8400-e29b-41d4-a716-446655440000'
ORDER BY name;

DROP TABLE t2134;
