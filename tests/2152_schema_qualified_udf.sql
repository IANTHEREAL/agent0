-- Regression test for issue #2152: schema-qualified UDF resolution.
-- Calling a function via schema.function_name() must work for any user schema,
-- not just the hardcoded cron/auth schemas.

DROP FUNCTION IF EXISTS test_schema_udf.add_one;
DROP SCHEMA IF EXISTS test_schema_udf CASCADE;

-- Create a user schema and a simple function in it.
CREATE SCHEMA test_schema_udf;

CREATE FUNCTION test_schema_udf.add_one(x int) RETURNS int
LANGUAGE sql AS $$ SELECT x + 1 $$;

-- Call with fully qualified name.
SELECT test_schema_udf.add_one(41);

-- Create a second function to ensure multiple functions work.
CREATE FUNCTION test_schema_udf.greet(name text) RETURNS text
LANGUAGE sql AS $$ SELECT 'hello ' || name $$;

SELECT test_schema_udf.greet('world');

-- Cleanup.
DROP FUNCTION test_schema_udf.add_one;
DROP FUNCTION test_schema_udf.greet;
DROP SCHEMA test_schema_udf;
