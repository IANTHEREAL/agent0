-- LATERAL JOIN Tests

DROP TABLE IF EXISTS departments CASCADE;
DROP TABLE IF EXISTS employees CASCADE;

CREATE TABLE departments (
    id INT PRIMARY KEY,
    name TEXT NOT NULL
);

CREATE TABLE employees (
    id INT PRIMARY KEY,
    dept_id INT REFERENCES departments(id),
    name TEXT NOT NULL,
    salary INT NOT NULL
);

INSERT INTO departments VALUES (1, 'Engineering'), (2, 'Sales'), (3, 'HR');
INSERT INTO employees VALUES 
    (1, 1, 'Alice', 90000),
    (2, 1, 'Bob', 85000),
    (3, 1, 'Charlie', 75000),
    (4, 2, 'Diana', 80000),
    (5, 2, 'Eve', 70000),
    (6, 3, 'Frank', 60000);

SELECT d.name AS dept, e.name AS emp, e.salary
FROM departments d
CROSS JOIN LATERAL (
    SELECT name, salary FROM employees WHERE dept_id = d.id ORDER BY salary DESC LIMIT 2
) e
ORDER BY d.name, e.salary DESC;

SELECT d.name AS dept, top_emp.name AS top_earner, top_emp.salary
FROM departments d
LEFT JOIN LATERAL (
    SELECT name, salary FROM employees WHERE dept_id = d.id ORDER BY salary DESC LIMIT 1
) top_emp ON true
ORDER BY d.name;

SELECT d.name AS dept, stats.emp_count, stats.avg_salary
FROM departments d
CROSS JOIN LATERAL (
    SELECT 
        COUNT(*) AS emp_count,
        AVG(salary)::INT AS avg_salary
    FROM employees 
    WHERE dept_id = d.id
) stats
ORDER BY d.name;

SELECT d.name AS dept, e.name AS emp, e.salary, avg_sal.avg_dept_salary
FROM departments d
CROSS JOIN LATERAL (
    SELECT AVG(salary)::INT AS avg_dept_salary FROM employees WHERE dept_id = d.id
) avg_sal
JOIN employees e ON e.dept_id = d.id AND e.salary > avg_sal.avg_dept_salary
ORDER BY d.name, e.salary DESC;

DROP TABLE employees;
DROP TABLE departments;

SELECT 'LATERAL JOIN tests completed' AS result;
