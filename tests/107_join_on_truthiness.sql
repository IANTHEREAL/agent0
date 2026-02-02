-- Issue #79 regression: JOIN ... ON must coerce boolean-ish text like WHERE does.

DROP TABLE IF EXISTS join_truth_left;
DROP TABLE IF EXISTS join_truth_right;

CREATE TABLE join_truth_left(id INT);
CREATE TABLE join_truth_right(id INT);

INSERT INTO join_truth_left VALUES (1), (2);
INSERT INTO join_truth_right VALUES (10), (20);

-- ON 'true' should match.
SELECT l.id AS l_id, r.id AS r_id
FROM join_truth_left l
JOIN join_truth_right r ON 'true'
ORDER BY l.id, r.id;

-- ON 'false' should not match.
SELECT l.id AS l_id, r.id AS r_id
FROM join_truth_left l
JOIN join_truth_right r ON 'false'
ORDER BY l.id, r.id;

-- ON 'notabool' should error.
SELECT l.id AS l_id, r.id AS r_id
FROM join_truth_left l
JOIN join_truth_right r ON 'notabool'
ORDER BY l.id, r.id;

DROP TABLE join_truth_left;
DROP TABLE join_truth_right;
