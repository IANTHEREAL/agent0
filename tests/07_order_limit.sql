-- Order By and Limit Tests
-- Purpose: Verify Sorting and Pagination

DROP TABLE IF EXISTS scores;
CREATE TABLE scores (
    name TEXT,
    score INT
);

INSERT INTO scores (name, score) VALUES ('Alice', 80);
INSERT INTO scores (name, score) VALUES ('Bob', 90);
INSERT INTO scores (name, score) VALUES ('Charlie', 70);
INSERT INTO scores (name, score) VALUES ('David', 85);
INSERT INTO scores (name, score) VALUES ('Eve', 95);

-- 1. Sort ASC
SELECT * FROM scores ORDER BY score;

-- 2. Sort DESC
SELECT * FROM scores ORDER BY score DESC;

-- 3. Top 2
SELECT * FROM scores ORDER BY score DESC LIMIT 2;

-- 4. Pagination (Skip top 2, take next 2)
SELECT * FROM scores ORDER BY score DESC LIMIT 2 OFFSET 2;

-- 5. LIMIT NULL → returns all rows (PG 17 parity: LIMIT NULL ≡ LIMIT ALL)
SELECT * FROM scores ORDER BY score LIMIT NULL;

-- 6. LIMIT NULL OFFSET NULL → returns all rows
SELECT * FROM scores ORDER BY score LIMIT NULL OFFSET NULL;

-- 7. LIMIT NULL OFFSET 2 → skip 2, return rest
SELECT * FROM scores ORDER BY score LIMIT NULL OFFSET 2;

DROP TABLE scores;
