-- Issue #1489: JSONB || operator combinations.
-- Covers all type-combination behaviors per PostgreSQL semantics.

-- 1. object || object → merged object
SELECT '{"a": 1}'::jsonb || '{"b": 2}'::jsonb AS obj_obj;

-- 2. object || object with key override
SELECT '{"a": 1, "b": 2}'::jsonb || '{"b": 99}'::jsonb AS obj_override;

-- 3. array || array → concatenated array
SELECT '[1, 2]'::jsonb || '[3, 4]'::jsonb AS arr_arr;

-- 4. scalar || scalar → creates array
SELECT '"hello"'::jsonb || '"world"'::jsonb AS scalar_scalar;

-- 5. number || number → creates array
SELECT '1'::jsonb || '2'::jsonb AS num_num;

-- 6. object || array → wraps object and concatenates
SELECT '{"a": 1}'::jsonb || '[2, 3]'::jsonb AS obj_arr;

-- 7. array || object → appends object to array
SELECT '[1, 2]'::jsonb || '{"a": 3}'::jsonb AS arr_obj;

-- 8. scalar || array → wraps scalar and concatenates
SELECT '"x"'::jsonb || '[1, 2]'::jsonb AS scalar_arr;

-- 9. array || scalar → appends scalar to array
SELECT '[1, 2]'::jsonb || '"x"'::jsonb AS arr_scalar;

-- 10. Empty object cases
SELECT '{}'::jsonb || '{"a": 1}'::jsonb AS empty_obj_merge;
SELECT '{"a": 1}'::jsonb || '{}'::jsonb AS obj_empty_merge;

-- 11. Empty array cases
SELECT '[]'::jsonb || '[1, 2]'::jsonb AS empty_arr_concat;
SELECT '[1, 2]'::jsonb || '[]'::jsonb AS arr_empty_concat;

-- 12. Multiple chained || operations
SELECT '{"a": 1}'::jsonb || '{"b": 2}'::jsonb || '{"c": 3}'::jsonb AS chained;

-- 13. UPDATE ... SET col = col || '...'::jsonb pattern (common ORM usage)
DROP TABLE IF EXISTS jsonb_settings CASCADE;
CREATE TABLE jsonb_settings (
    id INT PRIMARY KEY,
    config JSONB NOT NULL
);

INSERT INTO jsonb_settings VALUES (1, '{"theme": "dark", "lang": "en"}');

UPDATE jsonb_settings SET config = config || '{"lang": "fr", "timezone": "UTC"}'::jsonb WHERE id = 1;

SELECT * FROM jsonb_settings ORDER BY id;

-- 14. null || object and object || null
SELECT 'null'::jsonb || '{"a": 1}'::jsonb AS null_obj;
SELECT '{"a": 1}'::jsonb || 'null'::jsonb AS obj_null;

DROP TABLE jsonb_settings;
