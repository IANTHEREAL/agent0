-- Verify json[b]_each[_text] error messages match PostgreSQL 17.7 for non-object input.
-- PG 17.7 reference (array input):
--   jsonb_each('[1,2,3]')      → cannot call jsonb_each on a non-object
--   jsonb_each_text('[1,2,3]') → cannot call jsonb_each_text on a non-object
--   json_each('[1,2,3]')       → cannot deconstruct an array as an object
--   json_each_text('[1,2,3]')  → cannot deconstruct an array as an object
-- PG 17.7 reference (scalar input):
--   jsonb_each('1')            → cannot call jsonb_each on a non-object
--   jsonb_each_text('1')       → cannot call jsonb_each_text on a non-object
--   json_each('1')             → cannot deconstruct a scalar
--   json_each_text('1')        → cannot deconstruct a scalar

-- Expression context — array input
SELECT jsonb_each('[1,2,3]'::jsonb);
SELECT jsonb_each_text('[1,2,3]'::jsonb);
SELECT json_each('[1,2,3]'::json);
SELECT json_each_text('[1,2,3]'::json);

-- FROM-clause context — array input
SELECT * FROM jsonb_each('[1,2,3]'::jsonb);
SELECT * FROM jsonb_each_text('[1,2,3]'::jsonb);
SELECT * FROM json_each('[1,2,3]'::json);
SELECT * FROM json_each_text('[1,2,3]'::json);

-- Expression context — scalar input
SELECT jsonb_each('1'::jsonb);
SELECT jsonb_each_text('1'::jsonb);
SELECT json_each('1'::json);
SELECT json_each_text('1'::json);

-- FROM-clause context — scalar input
SELECT * FROM jsonb_each('1'::jsonb);
SELECT * FROM jsonb_each_text('1'::jsonb);
SELECT * FROM json_each('1'::json);
SELECT * FROM json_each_text('1'::json);
