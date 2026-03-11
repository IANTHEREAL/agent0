-- Issue #1547: json_array_elements* with AS alias (no column list)
-- should expose 'value' as column name, not the alias name.

-- 1. json_array_elements with alias, no column list → column name must be "value"
SELECT * FROM json_array_elements('[1, 2, 3]'::json) AS items;

-- 2. jsonb_array_elements with alias, no column list → column name must be "value"
SELECT * FROM jsonb_array_elements('[1, 2, 3]'::jsonb) AS items;

-- 3. json_array_elements_text with alias, no column list → column name must be "value"
SELECT * FROM json_array_elements_text('["a", "b"]'::json) AS items;

-- 4. jsonb_array_elements_text with alias, no column list → column name must be "value"
SELECT * FROM jsonb_array_elements_text('["a", "b"]'::jsonb) AS items;

-- 5. Explicit column list still renames → column name must be "v"
SELECT * FROM json_array_elements('[1, 2]'::json) AS items(v);

-- 6. Qualified reference with alias must work: items.value
SELECT items.value FROM json_array_elements('[10, 20]'::json) AS items;
