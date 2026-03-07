DROP TABLE IF EXISTS t_copy_csv_header_1508;
CREATE TABLE t_copy_csv_header_1508 (id INT, name TEXT);

COPY t_copy_csv_header_1508 (id, name) FROM STDIN WITH CSV HEADER;
id,name
1,Alice
2,Bob
\.

SELECT 'legacy_sum=' || COALESCE(SUM(id), 0) FROM t_copy_csv_header_1508;

TRUNCATE t_copy_csv_header_1508;

COPY t_copy_csv_header_1508 (id, name) FROM STDIN WITH (FORMAT csv, HEADER true);
id,name
3,Carol
4,Dan
\.

SELECT 'modern_names=' || array_to_string(array_agg(name ORDER BY id), ',') FROM t_copy_csv_header_1508;
SELECT 'modern_count=' || COUNT(*) FROM t_copy_csv_header_1508;

COPY t_copy_csv_header_1508 (id, name) FROM STDIN WITH (FORMAT csv, FORMAT text);
COPY t_copy_csv_header_1508 (id, name) FROM STDIN WITH (DELIMITER ',', DELIMITER '|');
COPY t_copy_csv_header_1508 (id, name) FROM STDIN WITH (FORMAT csv) CSV;

DROP TABLE t_copy_csv_header_1508;
