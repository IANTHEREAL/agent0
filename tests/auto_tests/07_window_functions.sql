-- Auto tests: Window Functions
CREATE SCHEMA IF NOT EXISTS auto_tests;
SET search_path TO auto_tests, public;

DROP TABLE IF EXISTS win_scores;

CREATE TABLE win_scores (
    grp TEXT,
    score INT
);

INSERT INTO win_scores (grp, score) VALUES
    ('A', 10),
    ('A', 20),
    ('A', 15),
    ('B', 5),
    ('B', 25);

SELECT grp, score,
       ROW_NUMBER() OVER (PARTITION BY grp ORDER BY score DESC) AS rn,
       RANK() OVER (PARTITION BY grp ORDER BY score DESC) AS rnk,
       DENSE_RANK() OVER (PARTITION BY grp ORDER BY score DESC) AS drnk
FROM win_scores
ORDER BY grp, score DESC;

SELECT grp, score,
       LAG(score) OVER (PARTITION BY grp ORDER BY score) AS prev_score,
       LEAD(score) OVER (PARTITION BY grp ORDER BY score) AS next_score
FROM win_scores
ORDER BY grp, score;

SELECT grp, score,
       SUM(score) OVER (PARTITION BY grp ORDER BY score) AS running_sum
FROM win_scores
ORDER BY grp, score;

DROP TABLE win_scores;
