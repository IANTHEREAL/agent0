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
SELECT to_regtype('interval garbage');
SELECT to_regtype('interval(abc)');
SELECT to_regtype('interval(999)');
SELECT to_regtype('interval(-1)');

-- Existing behavior regression
SELECT to_regtype('integer');
SELECT to_regtype('integer[]');
SELECT to_regtype('hstore');
SELECT to_regtype('hstore[]');
SELECT to_regtype('varchar(5)');
