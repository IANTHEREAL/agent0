-- Advanced JSON/JSONB Operations Tests

SELECT '{"name": "Alice", "age": 30}'::JSONB AS jsonb_literal;
SELECT '["a", "b", "c"]'::JSONB AS jsonb_array;
SELECT '{"nested": {"key": "value"}}'::JSONB AS nested_json;

SELECT '{"a": 1, "b": 2}'::JSONB -> 'a' AS get_key;
SELECT '{"a": 1, "b": 2}'::JSONB ->> 'a' AS get_key_text;
SELECT '["x", "y", "z"]'::JSONB -> 0 AS get_index;
SELECT '["x", "y", "z"]'::JSONB ->> 1 AS get_index_text;
SELECT '{"outer": {"inner": "value"}}'::JSONB -> 'outer' -> 'inner' AS nested_access;
SELECT '{"outer": {"inner": "value"}}'::JSONB #> '{outer,inner}' AS path_access;
SELECT '{"outer": {"inner": "value"}}'::JSONB #>> '{outer,inner}' AS path_access_text;

SELECT JSONB_OBJECT_KEYS('{"a": 1, "b": 2, "c": 3}'::JSONB) AS keys;
SELECT JSONB_ARRAY_ELEMENTS('["a", "b", "c"]'::JSONB) AS elements;
SELECT JSONB_ARRAY_ELEMENTS_TEXT('[1, 2, 3]'::JSONB) AS text_elements;

SELECT JSONB_EACH('{"a": 1, "b": 2}'::JSONB) AS each_pair;
SELECT JSONB_EACH_TEXT('{"a": 1, "b": 2}'::JSONB) AS each_text;

SELECT JSONB_TYPEOF('123'::JSONB) AS number_type;
SELECT JSONB_TYPEOF('"hello"'::JSONB) AS string_type;
SELECT JSONB_TYPEOF('true'::JSONB) AS boolean_type;
SELECT JSONB_TYPEOF('null'::JSONB) AS null_type;
SELECT JSONB_TYPEOF('{"a": 1}'::JSONB) AS object_type;
SELECT JSONB_TYPEOF('[1, 2]'::JSONB) AS array_type;

SELECT JSONB_ARRAY_LENGTH('[1, 2, 3, 4, 5]'::JSONB) AS arr_len;

SELECT '{"a": 1}'::JSONB || '{"b": 2}'::JSONB AS merged;
SELECT '{"a": 1, "b": 2}'::JSONB || '{"b": 3}'::JSONB AS override;

SELECT '{"a": 1, "b": 2}'::JSONB - 'a' AS removed_key;
SELECT '["a", "b", "c", "d"]'::JSONB - 1 AS removed_index;
SELECT '{"a": {"b": 1}}'::JSONB #- '{a,b}' AS removed_path;

SELECT JSONB_SET('{"a": 1}'::JSONB, '{b}', '2'::JSONB) AS added_key;
SELECT JSONB_SET('{"a": {"b": 1}}'::JSONB, '{a,b}', '99'::JSONB) AS updated_nested;
SELECT JSONB_SET('{"a": 1}'::JSONB, '{c}', '3'::JSONB, true) AS create_missing;

SELECT JSONB_BUILD_OBJECT('name', 'Alice', 'age', 30) AS built_object;
SELECT JSONB_BUILD_ARRAY(1, 2, 'three', true) AS built_array;

SELECT '{"a": 1}'::JSONB ? 'a' AS has_key;
SELECT '{"a": 1}'::JSONB ? 'b' AS missing_key;
SELECT '{"a": 1, "b": 2}'::JSONB ?| ARRAY['a', 'c'] AS has_any;
SELECT '{"a": 1, "b": 2}'::JSONB ?& ARRAY['a', 'b'] AS has_all;

SELECT '{"a": 1}'::JSONB @> '{"a": 1}'::JSONB AS contains;
SELECT '{"a": 1, "b": 2}'::JSONB @> '{"a": 1}'::JSONB AS contains_subset;
SELECT '{"a": 1}'::JSONB <@ '{"a": 1, "b": 2}'::JSONB AS contained_by;

SELECT TO_JSONB(ROW(1, 'test')) AS row_to_jsonb;
SELECT ROW_TO_JSON(ROW(1, 'test')) AS row_to_json;

DROP TABLE IF EXISTS products CASCADE;
CREATE TABLE products (
    id INT PRIMARY KEY,
    data JSONB
);

INSERT INTO products VALUES 
    (1, '{"name": "Widget", "price": 9.99, "tags": ["sale", "new"]}'),
    (2, '{"name": "Gadget", "price": 19.99, "tags": ["featured"]}'),
    (3, '{"name": "Gizmo", "price": 14.99, "tags": ["sale", "featured"]}');

SELECT id, data->>'name' AS name, (data->>'price')::NUMERIC AS price
FROM products
WHERE (data->>'price')::NUMERIC < 15
ORDER BY id;

SELECT id, data->>'name' AS name
FROM products
WHERE data->'tags' ? 'sale'
ORDER BY id;

SELECT id, JSONB_ARRAY_LENGTH(data->'tags') AS tag_count
FROM products
ORDER BY id;

DROP TABLE products;

SELECT 'Advanced JSON tests completed' AS result;
