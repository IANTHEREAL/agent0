-- to_regtype parity: _typename array aliases + interval qualifier forms (#1333)

-- Gap 1: _typename aliases
SELECT to_regtype('_int4');
SELECT to_regtype('_bool');
SELECT to_regtype('_text');
SELECT to_regtype('_hstore');
SELECT to_regtype('pg_catalog._int4');
SELECT to_regtype('_nonexistent');

-- Gap 2: interval qualifiers
SELECT to_regtype('interval day to second');
SELECT to_regtype('interval hour');
SELECT to_regtype('interval year to month');
SELECT to_regtype('interval(3)');
SELECT to_regtype('interval garbage'); -- db9-specific: error text differs from PG
SELECT to_regtype('interval(abc)'); -- db9-specific: error text differs from PG
SELECT to_regtype('interval(999)'); -- db9-specific: db9 suppresses interval precision warning
SELECT to_regtype('interval(2147483648)'); -- db9-specific: error text differs from PG
SELECT to_regtype('interval(-1)'); -- db9-specific: error text differs from PG

-- Quoted _typename aliases: case-sensitive, must NOT match alias list
SELECT to_regtype('"_INT4"');
SELECT to_regtype('pg_catalog."_INT4"');

-- Quoted schema case-sensitivity: quoted preserves case
SELECT to_regtype('"PG_CATALOG".int4');
SELECT to_regtype('"pg_catalog".int4');

-- Interval whitespace normalization
SELECT to_regtype('interval  day   to   second');
SELECT to_regtype('interval (3)');

-- Schema resolution: unknown schema + valid typmod structure → NULL
SELECT to_regtype('noschema.interval(abc)');
SELECT to_regtype('"PG_CATALOG".interval(abc)');
-- Schema-qualified interval qualifier → syntax error (PG invalid type name)
SELECT to_regtype('pg_catalog.interval day to second');
-- Schema resolution: unknown schema + bare word → syntax error propagates
SELECT to_regtype('noschema.interval day to second'); -- db9-specific: error text differs from PG
SELECT to_regtype('noschema.interval garbage'); -- db9-specific: error text differs from PG
SELECT to_regtype('noschema.int4 garbage'); -- db9-specific: error text differs from PG
SELECT to_regtype('noschema.foo garbage'); -- db9-specific: error text differs from PG

-- Unqualified bare-word trailing junk → syntax error
SELECT to_regtype('int4 garbage'); -- db9-specific: error text differs from PG
SELECT to_regtype('text garbage'); -- db9-specific: error text differs from PG

-- Quoted search_path schema must not match hstore fallback (case-sensitive)
SET search_path TO "Public";
SELECT to_regtype('hstore');
SET search_path TO public;

-- Multi-word PG type names must resolve, not error as trailing junk
SELECT to_regtype('double precision');
SELECT to_regtype('character varying');
SELECT to_regtype('character varying(255)');
SELECT to_regtype('timestamp with time zone');
SELECT to_regtype('timestamp without time zone');
SELECT to_regtype('time without time zone');
SELECT to_regtype('Double Precision');

-- Existing behavior regression
SELECT to_regtype('integer');
SELECT to_regtype('integer[]');
-- PG parity: without CREATE EXTENSION hstore, hstore regtype lookups are NULL.
SELECT to_regtype('hstore');
SELECT to_regtype('hstore[]');
SELECT to_regtype('varchar(5)');

-- Typmod semantic bounds validation (PG parity: #1425)
SELECT to_regtype('varchar(0)');
SELECT to_regtype('character(0)');
SELECT to_regtype('numeric(0)');
SELECT to_regtype('numeric(1001)');
SELECT to_regtype('numeric(10,11)');
SELECT to_regtype('pg_catalog.varchar(0)');
SELECT to_regtype('pg_catalog.character(0)');
SELECT to_regtype('pg_catalog.character(5)');
SELECT to_regtype('pg_catalog.decimal(10,2)');
SELECT to_regtype('pg_catalog.numeric(0)');
SELECT to_regtype('pg_catalog.numeric(1001)');
SELECT to_regtype('pg_catalog.numeric(10,11)');
SELECT to_regtype('numeric(10,-2)');
SELECT to_regtype('pg_catalog.numeric(10,-2)');
-- Negative precision for temporal types → ERROR (PG parity: #1425)
SELECT to_regtype('time(-1)');
SELECT to_regtype('timestamp(-1)');
SELECT to_regtype('timestamptz(-1)');
SELECT to_regtype('pg_catalog.time(-1)');
SELECT to_regtype('pg_catalog.timestamp(-1)');
SELECT to_regtype('pg_catalog.timestamptz(-1)');
-- Empty typmod (parens present, no arguments) → ERROR (PG parity: #1425)
SELECT to_regtype('pg_catalog.character()');
SELECT to_regtype('pg_catalog.decimal()');
-- Valid boundary values (should resolve to OIDs)
SELECT to_regtype('varchar(1)');
SELECT to_regtype('numeric(1)');
SELECT to_regtype('numeric(1000)');
SELECT to_regtype('numeric(10,10)');
