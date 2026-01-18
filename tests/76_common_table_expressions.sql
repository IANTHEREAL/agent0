-- Advanced Common Table Expressions Tests

DROP TABLE IF EXISTS employees CASCADE;

CREATE TABLE employees (
    id INT PRIMARY KEY,
    name TEXT NOT NULL,
    manager_id INT REFERENCES employees(id),
    salary INT NOT NULL
);

INSERT INTO employees VALUES
    (1, 'CEO', NULL, 200000),
    (2, 'VP Engineering', 1, 150000),
    (3, 'VP Sales', 1, 140000),
    (4, 'Dev Manager', 2, 120000),
    (5, 'Sales Manager', 3, 110000),
    (6, 'Senior Dev', 4, 90000),
    (7, 'Junior Dev', 4, 70000),
    (8, 'Sales Rep', 5, 60000);

WITH RECURSIVE org_tree AS (
    SELECT id, name, manager_id, 1 AS level, name::TEXT AS path
    FROM employees
    WHERE manager_id IS NULL
    
    UNION ALL
    
    SELECT e.id, e.name, e.manager_id, t.level + 1, t.path || ' > ' || e.name
    FROM employees e
    INNER JOIN org_tree t ON e.manager_id = t.id
)
SELECT id, name, level, path
FROM org_tree
ORDER BY path;

WITH RECURSIVE subordinates AS (
    SELECT id, name, manager_id
    FROM employees
    WHERE id = 2
    
    UNION ALL
    
    SELECT e.id, e.name, e.manager_id
    FROM employees e
    INNER JOIN subordinates s ON e.manager_id = s.id
)
SELECT id, name FROM subordinates ORDER BY id;

WITH 
avg_salary AS (
    SELECT AVG(salary)::INT AS avg_sal FROM employees
),
above_avg AS (
    SELECT * FROM employees WHERE salary > (SELECT avg_sal FROM avg_salary)
)
SELECT name, salary FROM above_avg ORDER BY salary DESC;

WITH top_earners AS (
    SELECT name, salary, RANK() OVER (ORDER BY salary DESC) AS rank
    FROM employees
)
SELECT name, salary, rank
FROM top_earners
WHERE rank <= 3
ORDER BY rank;

DROP TABLE employees;

DROP TABLE IF EXISTS products CASCADE;
CREATE TABLE products (id INT PRIMARY KEY, name TEXT, category TEXT, price INT);

INSERT INTO products VALUES 
    (1, 'Widget A', 'Widgets', 100),
    (2, 'Widget B', 'Widgets', 120),
    (3, 'Gadget A', 'Gadgets', 200),
    (4, 'Gadget B', 'Gadgets', 180);

WITH product_stats AS MATERIALIZED (
    SELECT category, AVG(price)::INT AS avg_price, COUNT(*) AS cnt
    FROM products
    GROUP BY category
)
SELECT * FROM product_stats ORDER BY category;

DROP TABLE products;

SELECT 'Advanced CTE tests completed' AS result;
