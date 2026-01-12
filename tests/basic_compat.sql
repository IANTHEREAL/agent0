-- Basic Compatibility Tests for pg-tikv
-- Tests common SQL patterns and PostgreSQL features

-- ============================================
-- CLEANUP
-- ============================================
DROP MATERIALIZED VIEW IF EXISTS mv_transcripts;
DROP TABLE IF EXISTS members CASCADE;
DROP TABLE IF EXISTS teams CASCADE;
DROP TABLE IF EXISTS grades CASCADE;
DROP TABLE IF EXISTS courses CASCADE;
DROP TABLE IF EXISTS students CASCADE;
DROP TABLE IF EXISTS instruments CASCADE;
DROP TABLE IF EXISTS orchestral_sections CASCADE;
DROP TABLE IF EXISTS books CASCADE;
DROP TABLE IF EXISTS movies CASCADE;
DROP TABLE IF EXISTS categories CASCADE;
DROP TABLE IF EXISTS todos CASCADE;

-- ============================================
-- 1. BASIC TABLE CREATION (from Tables docs)
-- ============================================
SELECT '=== 1. BASIC TABLE CREATION ===' as test_section;

-- Simple table with identity primary key
CREATE TABLE movies (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name TEXT,
    description TEXT
);

-- Table with various data types
CREATE TABLE todos (
    id SERIAL PRIMARY KEY,
    task TEXT NOT NULL,
    is_complete BOOLEAN DEFAULT FALSE,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

SELECT 'Created movies and todos tables' as result;

-- ============================================
-- 2. INSERT DATA (from Tables docs)
-- ============================================
SELECT '=== 2. INSERT DATA ===' as test_section;

INSERT INTO movies (name, description) VALUES
    ('The Empire Strikes Back', 'After the Rebels are brutally overpowered by the Empire on the ice planet Hoth, Luke Skywalker begins Jedi training with Yoda.'),
    ('Return of the Jedi', 'After a daring mission to rescue Han Solo from Jabba the Hutt, the Rebels dispatch to Endor to destroy the second Death Star.');

INSERT INTO todos (task, is_complete) VALUES
    ('Learn SQL', TRUE),
    ('Build an app', FALSE),
    ('Deploy to production', FALSE);

SELECT * FROM movies ORDER BY id;
SELECT * FROM todos ORDER BY id;

-- ============================================
-- 3. FOREIGN KEYS AND RELATIONSHIPS
-- ============================================
SELECT '=== 3. FOREIGN KEYS ===' as test_section;

-- Categories for movies
CREATE TABLE categories (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name TEXT NOT NULL
);

INSERT INTO categories (name) VALUES ('Action'), ('Documentary'), ('Sci-Fi');

ALTER TABLE movies ADD COLUMN category_id BIGINT REFERENCES categories(id);

UPDATE movies SET category_id = 3 WHERE name LIKE '%Empire%';
UPDATE movies SET category_id = 3 WHERE name LIKE '%Jedi%';

SELECT m.name as movie, c.name as category 
FROM movies m 
LEFT JOIN categories c ON m.category_id = c.id
ORDER BY m.id;

-- ============================================
-- 4. ONE-TO-MANY JOINS (from Joins docs)
-- ============================================
SELECT '=== 4. ONE-TO-MANY JOINS ===' as test_section;

CREATE TABLE orchestral_sections (
    id SERIAL PRIMARY KEY,
    name TEXT
);

CREATE TABLE instruments (
    id SERIAL PRIMARY KEY,
    name TEXT,
    section_id INT REFERENCES orchestral_sections(id)
);

INSERT INTO orchestral_sections (name) VALUES ('strings'), ('woodwinds');
INSERT INTO instruments (name, section_id) VALUES 
    ('violin', 1), ('viola', 1), ('flute', 2), ('oboe', 2);

SELECT s.name as section, i.name as instrument
FROM orchestral_sections s
JOIN instruments i ON s.id = i.section_id
ORDER BY s.name, i.name;

-- ============================================
-- 5. MANY-TO-MANY JOINS (from Joins docs)
-- ============================================
SELECT '=== 5. MANY-TO-MANY JOINS ===' as test_section;

CREATE TABLE students (
    id SERIAL PRIMARY KEY,
    name TEXT,
    type TEXT
);

CREATE TABLE courses (
    id SERIAL PRIMARY KEY,
    title TEXT,
    code TEXT
);

CREATE TABLE grades (
    id SERIAL PRIMARY KEY,
    student_id INT REFERENCES students(id),
    course_id INT REFERENCES courses(id),
    result TEXT
);

INSERT INTO students (name, type) VALUES 
    ('Princess Leia', 'undergraduate'),
    ('Yoda', 'graduate'),
    ('Anakin Skywalker', 'graduate');

INSERT INTO courses (title, code) VALUES 
    ('Introduction to Postgres', 'PG101'),
    ('Authentication Theories', 'AUTH205'),
    ('Fundamentals of Supabase', 'SUP412');

INSERT INTO grades (student_id, course_id, result) VALUES 
    (1, 1, 'B+'), (1, 3, 'A+'),
    (2, 2, 'A'),
    (3, 1, 'A-'), (3, 2, 'A'), (3, 3, 'B-');

-- View-like query for transcripts
SELECT 
    students.name,
    students.type,
    courses.title,
    courses.code,
    grades.result
FROM grades
LEFT JOIN students ON grades.student_id = students.id
LEFT JOIN courses ON grades.course_id = courses.id
ORDER BY students.name, courses.code;

-- ============================================
-- 6. MANY-TO-MANY via JOIN TABLE (teams/members)
-- ============================================
SELECT '=== 6. MANY-TO-MANY TEAMS ===' as test_section;

DROP TABLE IF EXISTS members CASCADE;
DROP TABLE IF EXISTS teams CASCADE;

-- Reuse students table as users
CREATE TABLE teams (
    id SERIAL PRIMARY KEY,
    team_name TEXT
);

CREATE TABLE members (
    user_id INT REFERENCES students(id),
    team_id INT REFERENCES teams(id),
    PRIMARY KEY (user_id, team_id)
);

INSERT INTO teams (team_name) VALUES ('Rebels'), ('Jedi Council');
INSERT INTO members (user_id, team_id) VALUES 
    (1, 1), (1, 2),  -- Leia in both teams
    (2, 2),          -- Yoda in Jedi Council
    (3, 1);          -- Anakin in Rebels

SELECT t.team_name, s.name as member
FROM teams t
JOIN members m ON t.id = m.team_id
JOIN students s ON m.user_id = s.id
ORDER BY t.team_name, s.name;

-- ============================================
-- 7. JSON/JSONB DATA (from JSON docs)
-- ============================================
SELECT '=== 7. JSON/JSONB DATA ===' as test_section;

DROP TABLE IF EXISTS books CASCADE;

CREATE TABLE books (
    id SERIAL PRIMARY KEY,
    title TEXT,
    author TEXT,
    metadata JSONB
);

INSERT INTO books (title, author, metadata) VALUES
    ('The Poky Little Puppy', 'Janette Sebring Lowrey', 
     '{"description":"Puppy is slower than other, bigger animals.","price":5.95,"ages":[3,6]}'),
    ('The Tale of Peter Rabbit', 'Beatrix Potter', 
     '{"description":"Rabbit eats some vegetables.","price":4.49,"ages":[2,5]}'),
    ('Tootle', 'Gertrude Crampton', 
     '{"description":"Little toy train has big dreams.","price":3.99,"ages":[2,5]}'),
    ('Green Eggs and Ham', 'Dr. Seuss', 
     '{"description":"Sam has changing food preferences and eats unusually colored food.","price":7.49,"ages":[4,8]}'),
    ('Harry Potter and the Goblet of Fire', 'J.K. Rowling', 
     '{"description":"Fourth year of school starts, big drama ensues.","price":24.95,"ages":[10,99]}');

-- Query JSON with -> operator (returns JSONB)
SELECT 
    title,
    metadata -> 'price' as price_jsonb,
    metadata -> 'ages' -> 0 as low_age
FROM books
ORDER BY title;

-- Query JSON with ->> operator (returns TEXT)
SELECT 
    title,
    metadata ->> 'description' as description,
    metadata ->> 'price' as price_text
FROM books
WHERE (metadata ->> 'price')::FLOAT > 5.00
ORDER BY title;

-- ============================================
-- 8. VIEWS (from Tables docs)
-- ============================================
SELECT '=== 8. VIEWS ===' as test_section;

CREATE VIEW transcripts AS
SELECT
    students.name,
    students.type,
    courses.title,
    courses.code,
    grades.result
FROM grades
LEFT JOIN students ON grades.student_id = students.id
LEFT JOIN courses ON grades.course_id = courses.id;

SELECT * FROM transcripts ORDER BY name, code;

-- ============================================
-- 9. MATERIALIZED VIEWS (from Tables docs)
-- ============================================
SELECT '=== 9. MATERIALIZED VIEWS ===' as test_section;

CREATE MATERIALIZED VIEW mv_transcripts AS
SELECT
    students.name,
    students.type,
    courses.title,
    courses.code,
    grades.result
FROM grades
LEFT JOIN students ON grades.student_id = students.id
LEFT JOIN courses ON grades.course_id = courses.id;

SELECT * FROM mv_transcripts ORDER BY name, code;

-- Add new data and refresh
INSERT INTO grades (student_id, course_id, result) VALUES (2, 1, 'A+');

REFRESH MATERIALIZED VIEW mv_transcripts;

SELECT * FROM mv_transcripts WHERE name = 'Yoda' ORDER BY code;

-- ============================================
-- 10. AGGREGATE FUNCTIONS
-- ============================================
SELECT '=== 10. AGGREGATE FUNCTIONS ===' as test_section;

SELECT 
    students.name,
    COUNT(*) as course_count,
    STRING_AGG(grades.result, ', ' ORDER BY courses.code) as all_grades
FROM grades
JOIN students ON grades.student_id = students.id
JOIN courses ON grades.course_id = courses.id
GROUP BY students.name
ORDER BY students.name;

-- Average price from JSON
SELECT 
    AVG((metadata ->> 'price')::FLOAT) as avg_price,
    MIN((metadata ->> 'price')::FLOAT) as min_price,
    MAX((metadata ->> 'price')::FLOAT) as max_price
FROM books;

-- ============================================
-- 11. SUBQUERIES
-- ============================================
SELECT '=== 11. SUBQUERIES ===' as test_section;

-- Find students with above-average number of courses
SELECT name, course_count
FROM (
    SELECT 
        students.name,
        COUNT(*) as course_count
    FROM grades
    JOIN students ON grades.student_id = students.id
    GROUP BY students.name
) as student_courses
WHERE course_count >= (
    SELECT AVG(cnt) FROM (
        SELECT COUNT(*) as cnt
        FROM grades
        GROUP BY student_id
    ) as avg_courses
)
ORDER BY course_count DESC;

-- ============================================
-- 12. CASE EXPRESSIONS
-- ============================================
SELECT '=== 12. CASE EXPRESSIONS ===' as test_section;

SELECT 
    title,
    (metadata ->> 'price')::FLOAT as price,
    CASE 
        WHEN (metadata ->> 'price')::FLOAT < 5 THEN 'Budget'
        WHEN (metadata ->> 'price')::FLOAT < 10 THEN 'Standard'
        ELSE 'Premium'
    END as price_tier
FROM books
ORDER BY price;

-- ============================================
-- 13. WINDOW FUNCTIONS
-- ============================================
SELECT '=== 13. WINDOW FUNCTIONS ===' as test_section;

SELECT 
    students.name,
    courses.code,
    grades.result,
    ROW_NUMBER() OVER (PARTITION BY students.id ORDER BY courses.code) as course_num,
    COUNT(*) OVER (PARTITION BY students.id) as total_courses
FROM grades
JOIN students ON grades.student_id = students.id
JOIN courses ON grades.course_id = courses.id
ORDER BY students.name, courses.code;

-- ============================================
-- 14. UPSERT (INSERT ON CONFLICT)
-- ============================================
SELECT '=== 14. UPSERT ===' as test_section;

-- Create a table with unique constraint
DROP TABLE IF EXISTS user_settings CASCADE;
CREATE TABLE user_settings (
    user_id INT PRIMARY KEY,
    theme TEXT DEFAULT 'light',
    notifications BOOLEAN DEFAULT TRUE
);

INSERT INTO user_settings (user_id, theme) VALUES (1, 'dark');
INSERT INTO user_settings (user_id, theme) VALUES (1, 'light') 
    ON CONFLICT (user_id) DO UPDATE SET theme = EXCLUDED.theme;

SELECT * FROM user_settings;

-- Insert new, update existing
INSERT INTO user_settings (user_id, theme, notifications) VALUES 
    (1, 'system', FALSE),
    (2, 'dark', TRUE)
ON CONFLICT (user_id) DO UPDATE SET 
    theme = EXCLUDED.theme,
    notifications = EXCLUDED.notifications;

SELECT * FROM user_settings ORDER BY user_id;

-- ============================================
-- 15. CTE (Common Table Expressions)
-- ============================================
SELECT '=== 15. CTE ===' as test_section;

WITH student_stats AS (
    SELECT 
        students.id,
        students.name,
        COUNT(*) as course_count
    FROM grades
    JOIN students ON grades.student_id = students.id
    GROUP BY students.id, students.name
),
course_stats AS (
    SELECT 
        courses.id,
        courses.code,
        COUNT(*) as student_count
    FROM grades
    JOIN courses ON grades.course_id = courses.id
    GROUP BY courses.id, courses.code
)
SELECT 
    s.name,
    s.course_count,
    c.code as most_popular_course
FROM student_stats s
CROSS JOIN (
    SELECT code FROM course_stats ORDER BY student_count DESC LIMIT 1
) c
ORDER BY s.name;

-- ============================================
-- 16. RECURSIVE CTE
-- ============================================
SELECT '=== 16. RECURSIVE CTE ===' as test_section;

-- Create org hierarchy
DROP TABLE IF EXISTS employees CASCADE;
CREATE TABLE employees (
    id SERIAL PRIMARY KEY,
    name TEXT,
    manager_id INT REFERENCES employees(id)
);

INSERT INTO employees (name, manager_id) VALUES 
    ('CEO', NULL),
    ('VP Engineering', 1),
    ('VP Sales', 1),
    ('Lead Dev', 2),
    ('Senior Dev', 4),
    ('Sales Rep', 3);

WITH RECURSIVE org_tree AS (
    SELECT id, name, manager_id, 1 as level
    FROM employees
    WHERE manager_id IS NULL
    UNION ALL
    SELECT e.id, e.name, e.manager_id, t.level + 1
    FROM employees e
    JOIN org_tree t ON e.manager_id = t.id
)
SELECT name, level FROM org_tree ORDER BY level, name;

-- ============================================
-- 17. DISTINCT ON
-- ============================================
SELECT '=== 17. DISTINCT ON ===' as test_section;

-- Get first grade for each student (by course code)
SELECT DISTINCT ON (students.name)
    students.name,
    courses.code,
    grades.result
FROM grades
JOIN students ON grades.student_id = students.id
JOIN courses ON grades.course_id = courses.id
ORDER BY students.name, courses.code;

-- ============================================
-- 18. COALESCE AND NULLIF
-- ============================================
SELECT '=== 18. COALESCE AND NULLIF ===' as test_section;

SELECT 
    name,
    COALESCE(manager_id::TEXT, 'No Manager') as manager_status,
    NULLIF(manager_id, 1) as non_ceo_manager
FROM employees
ORDER BY id;

-- ============================================
-- 19. STRING FUNCTIONS
-- ============================================
SELECT '=== 19. STRING FUNCTIONS ===' as test_section;

SELECT 
    title,
    UPPER(author) as author_upper,
    LENGTH(title) as title_length,
    SUBSTRING(title, 1, 10) as title_short,
    REPLACE(title, 'the', 'THE') as title_replaced
FROM books
ORDER BY title;

-- ============================================
-- 20. DATE/TIME FUNCTIONS
-- ============================================
SELECT '=== 20. DATE/TIME FUNCTIONS ===' as test_section;

SELECT 
    task,
    created_at,
    DATE(created_at) as created_date,
    EXTRACT(YEAR FROM created_at) as year,
    EXTRACT(MONTH FROM created_at) as month
FROM todos
ORDER BY id;

-- ============================================
-- CLEANUP for re-runs
-- ============================================
SELECT '=== CLEANUP ===' as test_section;

DROP MATERIALIZED VIEW IF EXISTS mv_transcripts;
DROP VIEW IF EXISTS transcripts;
DROP TABLE IF EXISTS user_settings CASCADE;
DROP TABLE IF EXISTS employees CASCADE;
DROP TABLE IF EXISTS members CASCADE;
DROP TABLE IF EXISTS teams CASCADE;
DROP TABLE IF EXISTS grades CASCADE;
DROP TABLE IF EXISTS courses CASCADE;
DROP TABLE IF EXISTS students CASCADE;
DROP TABLE IF EXISTS instruments CASCADE;
DROP TABLE IF EXISTS orchestral_sections CASCADE;
DROP TABLE IF EXISTS books CASCADE;
DROP TABLE IF EXISTS movies CASCADE;
DROP TABLE IF EXISTS categories CASCADE;
DROP TABLE IF EXISTS todos CASCADE;

SELECT 'All Supabase compatibility tests completed!' as final_result;
