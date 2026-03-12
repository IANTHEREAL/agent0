-- Issue #1791: json_object_keys, json_array_elements, json_array_elements_text
-- must expand rows when called in the SELECT list (SRF detection parity).

-- 1. json_object_keys in SELECT list → row expansion
SELECT json_object_keys('{"a":1,"b":2}'::json);

-- 2. json_array_elements in SELECT list → row expansion
SELECT json_array_elements('[1,2,3]'::json);

-- 3. json_array_elements_text in SELECT list → row expansion
SELECT json_array_elements_text('["x","y"]'::json);
