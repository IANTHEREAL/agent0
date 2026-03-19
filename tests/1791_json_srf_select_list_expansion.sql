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

-- 8. json_object_keys preserves original key order (b before a)
-- PostgreSQL 17.9: returns b, a (not sorted)
SELECT json_object_keys('{"b":1,"a":2}'::json);

-- 9. json_array_elements_text preserves key order in object elements
-- PostgreSQL 17.9: returns {"b":1,"a":2} (not JSONB-canonicalized {"a":2,"b":1})
SELECT json_array_elements_text('[{"b":1,"a":2}]'::json);

-- 10. json_array_elements_text with mixed element types
SELECT json_array_elements_text('[1,"hello",null,true,{"z":1,"a":2}]'::json);

-- 11. Error: json_object_keys on an array → PG: "cannot call json_object_keys on an array"
SELECT json_object_keys('[1,2]'::json);

-- 12. Error: json_object_keys on a scalar → PG: "cannot call json_object_keys on a scalar"
SELECT json_object_keys('"hello"'::json);

-- 13. Error: json_array_elements on a non-array → PG: "cannot call json_array_elements on a non-array"
SELECT json_array_elements('{"a":1}'::json);

-- 14. Error: json_array_elements_text on a non-array → PG: "cannot call json_array_elements_text on a non-array"
SELECT json_array_elements_text('{"a":1}'::json);

-- 15. Error: json_object_keys on jsonb input → PG: "function json_object_keys(jsonb) does not exist"
SELECT json_object_keys('{"a":1}'::jsonb);

-- 16. Error: json_array_elements on jsonb input → PG: "function json_array_elements(jsonb) does not exist"
SELECT json_array_elements('[1,2]'::jsonb);

-- 17. Error: json_array_elements_text on jsonb input → PG: "function json_array_elements_text(jsonb) does not exist"
SELECT json_array_elements_text('[1,2]'::jsonb);
