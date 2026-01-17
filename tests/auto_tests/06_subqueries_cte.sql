-- Auto tests: Subqueries / CTE
CREATE SCHEMA IF NOT EXISTS auto_tests;
SET search_path TO auto_tests, public;

DROP TABLE IF EXISTS subq_items;

CREATE TABLE subq_items (
    id INT PRIMARY KEY,
    category TEXT,
    amount INT
);

INSERT INTO subq_items (id, category, amount) VALUES
    (1, 'x', 10),
    (2, 'x', 20),
    (3, 'y', 5),
    (4, 'y', 15);

SELECT * FROM subq_items
WHERE id IN (SELECT id FROM subq_items WHERE amount >= 15)
ORDER BY id;

SELECT * FROM subq_items s
WHERE EXISTS (SELECT 1 FROM subq_items t WHERE t.category = s.category AND t.amount > 18)
ORDER BY id;

SELECT id,
       (SELECT COUNT(*) FROM subq_items t WHERE t.category = s.category) AS cat_count
FROM subq_items s
ORDER BY id;

WITH high_amount AS (
    SELECT id, amount FROM subq_items WHERE amount >= 15
)
SELECT * FROM high_amount ORDER BY id;

WITH RECURSIVE seq(n) AS (
    SELECT 1
    UNION ALL
    SELECT n + 1 FROM seq WHERE n < 3
)
SELECT n FROM seq ORDER BY n;

DROP TABLE subq_items;
