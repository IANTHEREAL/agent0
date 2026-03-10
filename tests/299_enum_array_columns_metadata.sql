-- Regression: information_schema.columns must render enum and enum[]
-- columns with correct data_type / udt_schema / udt_name values,
-- matching PostgreSQL behavior.
-- Fixes: #1697

DROP TABLE IF EXISTS t_enum_meta CASCADE;
DROP TYPE IF EXISTS mood_meta CASCADE;

CREATE TYPE mood_meta AS ENUM ('happy', 'sad', 'neutral');

CREATE TABLE t_enum_meta (
    id   SERIAL PRIMARY KEY,
    feel mood_meta,
    tags mood_meta[]
);

-- PostgreSQL returns:
--   feel  → data_type='USER-DEFINED', udt_schema='public', udt_name='mood_meta'
--   tags  → data_type='ARRAY',        udt_schema='public', udt_name='_mood_meta'
SELECT column_name, data_type, udt_schema, udt_name
FROM information_schema.columns
WHERE table_schema = 'public' AND table_name = 't_enum_meta'
  AND column_name IN ('feel', 'tags')
ORDER BY ordinal_position;

DROP TABLE t_enum_meta CASCADE;
DROP TYPE mood_meta CASCADE;
