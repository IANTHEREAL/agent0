-- Enum array validation: enum[] columns must validate each element label
-- at write time. Invalid labels must be rejected.

DROP TABLE IF EXISTS t_enum_arr CASCADE;
DROP TYPE IF EXISTS mood CASCADE;

CREATE TYPE mood AS ENUM ('happy', 'sad', 'neutral');

CREATE TABLE t_enum_arr (
    id SERIAL PRIMARY KEY,
    moods mood[] NOT NULL
);

-- Valid enum array insert.
INSERT INTO t_enum_arr (moods) VALUES (ARRAY['happy', 'sad']::mood[]);
SELECT moods FROM t_enum_arr ORDER BY id;

-- Invalid enum label in array → error.
INSERT INTO t_enum_arr (moods) VALUES (ARRAY['happy', 'angry']::mood[]);

-- NULL elements are allowed in PG.
INSERT INTO t_enum_arr (moods) VALUES (ARRAY['happy', NULL]::mood[]);
SELECT moods FROM t_enum_arr ORDER BY id;

DROP TABLE t_enum_arr CASCADE;
DROP TYPE mood CASCADE;
