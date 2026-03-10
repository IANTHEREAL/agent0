-- fs9 + parquet/CSV integration against embedded pagefs.
-- Fixture files are staged into fs9 by 244_fs9_parquet_integration_load.py.

CREATE EXTENSION IF NOT EXISTS fs9;
CREATE EXTENSION IF NOT EXISTS parquet;
SET search_path TO public, extensions;

-- Path 1: fs9() table function reads parquet file
SELECT * FROM fs9('tests/parquet_testdata/basic.parquet') ORDER BY id LIMIT 5;

-- Path 2: read_parquet() with fs9:// scheme
SELECT * FROM read_parquet('fs9://tests/parquet_testdata/basic.parquet') ORDER BY id LIMIT 5;

-- Path 3: COPY FROM fs9:// with FORMAT parquet
DROP TABLE IF EXISTS e2e_parquet_test;
CREATE TABLE e2e_parquet_test (id bigint, name text, value double precision);
COPY e2e_parquet_test FROM 'fs9://tests/parquet_testdata/basic.parquet' WITH (FORMAT parquet);
SELECT count(*) FROM e2e_parquet_test;

-- Path 4: COPY FROM fs9:// with FORMAT csv
DROP TABLE IF EXISTS e2e_csv_test;
CREATE TABLE e2e_csv_test (id int, name text, value text);
COPY e2e_csv_test FROM 'fs9://tests/parquet_testdata/test_copy.csv' WITH (FORMAT csv, HEADER true);
SELECT count(*) FROM e2e_csv_test;

-- CTAS from read_parquet(fs9://)
DROP TABLE IF EXISTS e2e_ctas_test;
CREATE TABLE e2e_ctas_test AS SELECT * FROM read_parquet('fs9://tests/parquet_testdata/basic.parquet');
SELECT count(*) FROM e2e_ctas_test;

-- Cleanup
DROP TABLE IF EXISTS e2e_parquet_test;
DROP TABLE IF EXISTS e2e_csv_test;
DROP TABLE IF EXISTS e2e_ctas_test;
SELECT CASE
    WHEN fs9_exists('tests/parquet_testdata/') THEN fs9_remove('tests/parquet_testdata/', true)
    ELSE 0
END;
DROP EXTENSION IF EXISTS parquet;
DROP EXTENSION IF EXISTS fs9;
