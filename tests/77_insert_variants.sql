DROP TABLE IF EXISTS insert_test CASCADE;
DROP TABLE IF EXISTS insert_source CASCADE;

CREATE TABLE insert_test (
    id INT PRIMARY KEY,
    name TEXT NOT NULL,
    value INT DEFAULT 0,
    created_at TIMESTAMP DEFAULT '2024-01-15 10:00:00'
);

INSERT INTO insert_test (id, name) VALUES (1, 'first');
INSERT INTO insert_test (id, name, value) VALUES (2, 'second', 100);
INSERT INTO insert_test VALUES (3, 'third', 200, '2024-01-16 12:00:00');

SELECT * FROM insert_test ORDER BY id;

INSERT INTO insert_test (id, name, value) VALUES 
    (4, 'fourth', 300),
    (5, 'fifth', 400),
    (6, 'sixth', 500);

SELECT * FROM insert_test ORDER BY id;

INSERT INTO insert_test (id, name, value) VALUES (10, 'tenth', 1000) RETURNING *;
INSERT INTO insert_test (id, name) VALUES (11, 'eleventh') RETURNING id, name;
INSERT INTO insert_test (id, name, value) VALUES (12, 'twelfth', 1200) RETURNING id AS new_id, name AS new_name;

CREATE TABLE insert_source (id INT PRIMARY KEY, name TEXT, value INT);
INSERT INTO insert_source VALUES (20, 'source_a', 2000), (21, 'source_b', 2100);

INSERT INTO insert_test (id, name, value)
SELECT id, name, value FROM insert_source;

SELECT * FROM insert_test WHERE id >= 20 ORDER BY id;

INSERT INTO insert_test (id, name, value)
SELECT id + 100, UPPER(name), value * 2 FROM insert_source
RETURNING *;

INSERT INTO insert_test (id, name, value) VALUES (7, 'conflict', 700)
ON CONFLICT (id) DO NOTHING;
INSERT INTO insert_test (id, name, value) VALUES (7, 'conflict_new', 777)
ON CONFLICT (id) DO NOTHING;

SELECT * FROM insert_test WHERE id = 7;

INSERT INTO insert_test (id, name, value) VALUES (8, 'update_test', 800);
INSERT INTO insert_test (id, name, value) VALUES (8, 'updated', 888)
ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name, value = EXCLUDED.value;

SELECT * FROM insert_test WHERE id = 8;

INSERT INTO insert_test (id, name, value) VALUES (9, 'upsert', 900)
ON CONFLICT (id) DO UPDATE SET value = insert_test.value + EXCLUDED.value
RETURNING *;

INSERT INTO insert_test (id, name, value) VALUES (9, 'upsert_add', 100)
ON CONFLICT (id) DO UPDATE SET value = insert_test.value + EXCLUDED.value
RETURNING *;

DROP TABLE insert_source;
DROP TABLE insert_test;

SELECT 'INSERT variants tests completed' AS result;
