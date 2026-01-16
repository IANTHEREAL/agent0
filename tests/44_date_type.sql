-- DATE type smoke test.

DROP TABLE IF EXISTS date_type_t;

CREATE TABLE date_type_t (id INT PRIMARY KEY, d DATE);

INSERT INTO date_type_t(id, d) VALUES (1, '2024-01-02');
INSERT INTO date_type_t(id, d) VALUES (2, DATE '2024-01-03');

SELECT id, d FROM date_type_t ORDER BY d;
SELECT id FROM date_type_t WHERE d >= '2024-01-03' ORDER BY id;

SELECT data_type, udt_name
FROM information_schema.columns
WHERE table_name = 'date_type_t' AND column_name = 'd';

SELECT json_build_object('d', d) FROM date_type_t WHERE id = 1;

DROP TABLE date_type_t;

