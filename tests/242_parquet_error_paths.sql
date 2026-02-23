-- Parquet error paths: URL validation, SSRF, unsupported options

CREATE EXTENSION IF NOT EXISTS parquet;

-- Invalid URL scheme (ftp)
SELECT * FROM read_parquet('ftp://example.com/test.parquet');

-- Invalid URL scheme (file)
SELECT * FROM read_parquet('file:///tmp/test.parquet');

-- SSRF: localhost should be blocked
SELECT * FROM read_parquet('https://localhost/test.parquet');

-- SSRF: loopback IP should be blocked
SELECT * FROM read_parquet('https://127.0.0.1/test.parquet');

-- COPY with unsupported FORMAT parquet + DELIMITER option
COPY parquet_err_test FROM 'https://example.com/test.parquet' WITH (FORMAT parquet, DELIMITER ',');

-- COPY TO with FORMAT parquet should be rejected
CREATE TABLE parquet_err_test (id INT, name TEXT);
COPY parquet_err_test TO STDOUT WITH (FORMAT parquet);

-- COPY FROM PARQUET with column list should be rejected
COPY parquet_err_test (id) FROM 'https://example.com/test.parquet' WITH (FORMAT parquet);

DROP TABLE IF EXISTS parquet_err_test;
DROP EXTENSION IF EXISTS parquet;
