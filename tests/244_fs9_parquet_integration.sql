-- fs9 + parquet/CSV local integration tests
-- Requires:
--   1) db9-server started with --features parquet
--   2) fs9 local backend configured (default for local filesystem)
--   3) tests/parquet_testdata/basic.parquet (100 rows: id int32, name text, value float8)
--   4) tests/parquet_testdata/test_copy.csv (10 rows with header: id,name,value)

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
DROP EXTENSION IF EXISTS parquet;
DROP EXTENSION IF EXISTS fs9;
