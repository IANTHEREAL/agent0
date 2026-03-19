-- Issue #1791: json_object_keys, json_array_elements, json_array_elements_text
-- must expand rows when called in the SELECT list (SRF detection parity).

-- 1. json_object_keys in SELECT list → row expansion
SELECT json_object_keys('{"a":1,"b":2}'::json);

-- 2. json_array_elements in SELECT list → row expansion (numeric)
SELECT json_array_elements('[1,2,3]'::json);

-- 3. json_array_elements_text in SELECT list → row expansion
SELECT json_array_elements_text('["x","y"]'::json);

-- 4. json_array_elements with object/array elements preserves JSON key order
-- PostgreSQL returns {"b":1,"a":2} (original key order), not JSONB-canonicalized {"a":2,"b":1}
SELECT json_array_elements('[{"b":1,"a":2},[3,4]]'::json);

-- 5. json_array_elements with whitespace around elements (normalize per-element whitespace)
SELECT json_array_elements('[ {"a":1}, 2 ]'::json);

-- 6. Empty array with whitespace → zero rows (not one whitespace-only row)
SELECT json_array_elements('[ ]'::json);

-- 7. Empty array (minified) → zero rows
SELECT json_array_elements('[]'::json);
