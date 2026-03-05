-- to_regtype: timetz (time with time zone) must resolve to OID 1266.
-- Validates bare, long-form, schema-qualified, typmod, and invalid-precision variants.
-- Ref: issue #1447, #1453

SELECT to_regtype('timetz');
SELECT to_regtype('time with time zone');
SELECT to_regtype('pg_catalog.timetz');
SELECT to_regtype('pg_catalog.timetz(3)');
SELECT to_regtype('timetz[]');
-- Precision > 6 is clamped (PG returns OID with warning)
SELECT to_regtype('pg_catalog.timetz(7)');
SELECT to_regtype('timetz(7)');
-- Negative precision → ERROR (PG: TIME(-1) WITH TIME ZONE precision must not be negative)
SELECT to_regtype('pg_catalog.timetz(-1)');
-- Non-integer precision → ERROR (PG: invalid input syntax for type integer: "foo")
SELECT to_regtype('pg_catalog.timetz(foo)');
