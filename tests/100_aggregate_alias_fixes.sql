-- Integration tests for #562 (nested ARRAY_AGG), #563 (alias whole-row), #564 (legacy ARRAY_AGG ORDER BY)
-- Tracking issue: #566

-- Setup
DROP TABLE IF EXISTS aaf_items;
CREATE TABLE aaf_items (
    id INT PRIMARY KEY,
    category TEXT,
    name TEXT,
    score INT
);
INSERT INTO aaf_items VALUES (1, 'fruit', 'banana', 30);
INSERT INTO aaf_items VALUES (2, 'fruit', 'apple', 10);
INSERT INTO aaf_items VALUES (3, 'fruit', 'cherry', 20);
INSERT INTO aaf_items VALUES (4, 'veggie', 'carrot', 50);
INSERT INTO aaf_items VALUES (5, 'veggie', 'bean', 40);

---------------------------------------------------------------
-- Fix #562: Nested ARRAY_AGG inside expressions
---------------------------------------------------------------

-- ARRAY_AGG inside IS NULL / IS NOT NULL
SELECT category, ARRAY_AGG(name) IS NOT NULL AS has_names
FROM aaf_items GROUP BY category ORDER BY category;

-- ARRAY_AGG inside CASE
SELECT category,
       CASE WHEN ARRAY_AGG(name) IS NOT NULL THEN 'yes' ELSE 'no' END AS has_names
FROM aaf_items GROUP BY category ORDER BY category;

-- ARRAY_AGG inside COALESCE
SELECT COALESCE(ARRAY_AGG(name), ARRAY['none']) AS names
FROM aaf_items WHERE id < 0;

---------------------------------------------------------------
-- Fix #563: Whole-row reference by FROM alias
---------------------------------------------------------------

-- Simple whole-row reference by alias
SELECT bar FROM aaf_items AS bar WHERE bar.id = 1;

-- Qualified column via alias
SELECT bar.name FROM aaf_items AS bar WHERE bar.id = 1;

-- Alias with aggregate
SELECT bar.category, COUNT(*) AS cnt
FROM aaf_items AS bar
GROUP BY bar.category
ORDER BY bar.category;

---------------------------------------------------------------
-- Fix #564: ARRAY_AGG ORDER BY in legacy path
---------------------------------------------------------------

-- ARRAY_AGG with ORDER BY ASC
SELECT category, ARRAY_AGG(name ORDER BY name) AS sorted_names
FROM aaf_items GROUP BY category ORDER BY category;

-- ARRAY_AGG with ORDER BY DESC
SELECT category, ARRAY_AGG(name ORDER BY score DESC) AS names_by_score_desc
FROM aaf_items GROUP BY category ORDER BY category;

-- ARRAY_AGG with ORDER BY on different column
SELECT category, ARRAY_AGG(name ORDER BY score) AS names_by_score
FROM aaf_items GROUP BY category ORDER BY category;

-- Global ARRAY_AGG ORDER BY (no GROUP BY)
SELECT ARRAY_AGG(name ORDER BY name) AS all_sorted FROM aaf_items;

-- Cleanup
DROP TABLE IF EXISTS aaf_items;
