-- CASE Expression Tests

DROP TABLE IF EXISTS case_test;
CREATE TABLE case_test (
    id INT PRIMARY KEY,
    score INT,
    status TEXT,
    value NUMERIC(10,2)
);

INSERT INTO case_test VALUES (1, 95, 'active', 100.00);
INSERT INTO case_test VALUES (2, 75, 'inactive', 200.00);
INSERT INTO case_test VALUES (3, 55, 'active', 150.00);
INSERT INTO case_test VALUES (4, 85, 'pending', NULL);
INSERT INTO case_test VALUES (5, NULL, 'active', 300.00);

-- Simple CASE
SELECT id, score,
    CASE score
        WHEN 95 THEN 'A'
        WHEN 85 THEN 'B'
        WHEN 75 THEN 'C'
        ELSE 'Other'
    END AS grade
FROM case_test ORDER BY id;

-- Simple CASE NULL semantics (NULL never matches, even against NULL)
SELECT CASE NULL
    WHEN NULL THEN 'then_branch'
    ELSE 'else_branch'
END AS simple_case_null_operand;

SELECT CASE score
    WHEN NULL THEN 'then_branch'
    ELSE 'else_branch'
END AS simple_case_null_when
FROM case_test WHERE id = 5;

-- Searched CASE
SELECT id, score,
    CASE 
        WHEN score >= 90 THEN 'Excellent'
        WHEN score >= 80 THEN 'Good'
        WHEN score >= 70 THEN 'Average'
        WHEN score >= 60 THEN 'Below Average'
        ELSE 'Fail'
    END AS performance
FROM case_test ORDER BY id;

-- CASE with NULL handling
SELECT id, score,
    CASE 
        WHEN score IS NULL THEN 'No Score'
        WHEN score >= 80 THEN 'Pass'
        ELSE 'Fail'
    END AS result
FROM case_test ORDER BY id;

-- CASE in WHERE clause
SELECT id, score FROM case_test 
WHERE CASE WHEN score >= 80 THEN 'high' ELSE 'low' END = 'high'
ORDER BY id;

-- CASE with aggregation
SELECT 
    CASE 
        WHEN score >= 80 THEN 'High'
        WHEN score >= 60 THEN 'Medium'
        ELSE 'Low'
    END AS score_group,
    COUNT(*) AS cnt
FROM case_test 
WHERE score IS NOT NULL
GROUP BY CASE 
    WHEN score >= 80 THEN 'High'
    WHEN score >= 60 THEN 'Medium'
    ELSE 'Low'
END
ORDER BY score_group;

-- Nested CASE
SELECT id, status, score,
    CASE status
        WHEN 'active' THEN 
            CASE 
                WHEN score >= 80 THEN 'Active High'
                ELSE 'Active Low'
            END
        WHEN 'inactive' THEN 'Inactive'
        ELSE 'Other'
    END AS detailed_status
FROM case_test ORDER BY id;

-- CASE returning different types (should all be text)
SELECT id,
    CASE 
        WHEN value IS NULL THEN 'N/A'
        WHEN value > 200 THEN 'High'
        ELSE 'Normal'
    END AS value_category
FROM case_test ORDER BY id;

-- COALESCE (shorthand for CASE WHEN x IS NULL)
SELECT id, COALESCE(score, 0) AS score_or_zero FROM case_test ORDER BY id;
SELECT id, COALESCE(value, 0.00) AS value_or_zero FROM case_test ORDER BY id;
SELECT id, COALESCE(NULL, score, 999) AS first_non_null FROM case_test ORDER BY id;

-- NULLIF
SELECT id, NULLIF(score, 75) AS not_75 FROM case_test ORDER BY id;
SELECT id, NULLIF(status, 'inactive') AS not_inactive FROM case_test ORDER BY id;

-- GREATEST / LEAST
SELECT GREATEST(1, 5, 3, 9, 2) AS greatest_val;
SELECT LEAST(1, 5, 3, 9, 2) AS least_val;
SELECT id, GREATEST(score, 60) AS at_least_60 FROM case_test WHERE score IS NOT NULL ORDER BY id;

DROP TABLE case_test;

SELECT 'CASE expression tests completed' AS result;
