-- Regression test: UUID predicate compatibility (#2229)
-- 1. IS [NOT] DISTINCT FROM with AND/OR (parser precedence fix)
-- 2. UUID array ANY filter

-- ================================================================
-- 1. IS NOT DISTINCT FROM: parser precedence with AND/OR
-- ================================================================

CREATE TABLE indf_test (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name TEXT,
    ts TIMESTAMPTZ
);

INSERT INTO indf_test VALUES
    ('a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', 'alice', '2024-01-15 10:00:00+00'),
    ('b0eebc99-9c0b-4ef8-bb6d-6bb9bd380a22', 'bob', NULL),
    ('c0eebc99-9c0b-4ef8-bb6d-6bb9bd380a33', NULL, '2024-03-20 15:00:00+00');

-- 1a. IS NOT DISTINCT FROM with AND — the core bug
--     Previously parsed as: id IS NOT DISTINCT FROM (uuid AND name = 'alice')
SELECT id, name FROM indf_test
WHERE id IS NOT DISTINCT FROM 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid
  AND name = 'alice'
ORDER BY id;

-- 1b. IS NOT DISTINCT FROM with NULL comparison
SELECT id, name FROM indf_test
WHERE name IS NOT DISTINCT FROM NULL
ORDER BY id;

-- 1c. IS DISTINCT FROM with AND
SELECT id, name FROM indf_test
WHERE id IS DISTINCT FROM 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid
  AND name IS NOT NULL
ORDER BY id;

-- 1d. IS NOT DISTINCT FROM on TIMESTAMPTZ with OR
SELECT id, name FROM indf_test
WHERE ts IS NOT DISTINCT FROM '2024-01-15 10:00:00+00'::timestamptz
   OR ts IS NOT DISTINCT FROM NULL
ORDER BY id;

-- 1e. Multiple IS NOT DISTINCT FROM in one WHERE (Prisma pattern)
SELECT id FROM indf_test
WHERE id IS NOT DISTINCT FROM 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid
  AND name IS NOT DISTINCT FROM 'alice'
  AND ts IS NOT DISTINCT FROM '2024-01-15 10:00:00+00'::timestamptz;

-- 1f. IS NOT DISTINCT FROM with non-null values (equality behavior)
SELECT id FROM indf_test
WHERE name IS NOT DISTINCT FROM 'bob'
ORDER BY id;

-- 1g. Mixed IS NOT DISTINCT FROM and regular operators
SELECT id, name FROM indf_test
WHERE (id IS NOT DISTINCT FROM 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid OR name = 'bob')
  AND ts IS DISTINCT FROM NULL
ORDER BY id;

-- ================================================================
-- 2. UUID array ANY filter
-- ================================================================

-- 2a. Basic = ANY with UUID array literal
SELECT id, name FROM indf_test
WHERE id = ANY(ARRAY['a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid, 'b0eebc99-9c0b-4ef8-bb6d-6bb9bd380a22'::uuid])
ORDER BY id;

-- 2b. = ANY with UUID array cast from text
SELECT id, name FROM indf_test
WHERE id = ANY('{a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11,c0eebc99-9c0b-4ef8-bb6d-6bb9bd380a33}'::uuid[])
ORDER BY id;

-- 2c. NOT IN equivalent via <> ALL
SELECT id, name FROM indf_test
WHERE id <> ALL(ARRAY['a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid])
ORDER BY id;

-- 2d. Empty array
SELECT id FROM indf_test
WHERE id = ANY(ARRAY[]::uuid[])
ORDER BY id;

-- 2e. = ANY with single element
SELECT id, name FROM indf_test
WHERE id = ANY(ARRAY['b0eebc99-9c0b-4ef8-bb6d-6bb9bd380a22'::uuid])
ORDER BY id;

-- ================================================================
-- 3. Combined: IS NOT DISTINCT FROM + array ANY in same query
-- ================================================================

SELECT id, name FROM indf_test
WHERE id = ANY(ARRAY['a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid, 'b0eebc99-9c0b-4ef8-bb6d-6bb9bd380a22'::uuid])
  AND name IS NOT DISTINCT FROM 'alice'
ORDER BY id;

-- ================================================================
-- Cleanup
-- ================================================================

DROP TABLE indf_test;
