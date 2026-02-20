DROP TABLE IF EXISTS corr_test_data;

CREATE TABLE corr_test_data (id INT, n INT);
INSERT INTO corr_test_data VALUES (1, 3), (2, 5), (3, 2);

SELECT d.id, d.n, (SELECT count(*) FROM generate_series(1, d.n)) AS series_count
FROM corr_test_data d
ORDER BY d.id;

DROP TABLE corr_test_data;
