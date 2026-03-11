-- Regression: information_schema.columns.data_type must return 'ARRAY'
-- for all array columns, matching PostgreSQL semantics.
-- Fixes: #1725

DROP TABLE IF EXISTS t_array_meta CASCADE;

CREATE TABLE t_array_meta (
    id      SERIAL PRIMARY KEY,
    ints    integer[],
    bigs    bigint[],
    texts   text[],
    names   name[],
    vchars  varchar[]
);

SELECT column_name, data_type, udt_name
FROM information_schema.columns
WHERE table_schema = 'public' AND table_name = 't_array_meta'
  AND column_name IN ('ints', 'bigs', 'texts', 'names', 'vchars')
ORDER BY ordinal_position;

DROP TABLE t_array_meta CASCADE;
