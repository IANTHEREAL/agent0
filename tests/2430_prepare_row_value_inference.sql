-- Issue #2430: SQL PREPARE should infer parameter types for row-value IN lists
-- from the corresponding tuple positions, matching PostgreSQL.

DROP TABLE IF EXISTS prep_row_inference;
CREATE TABLE prep_row_inference (
    s_w_id INT NOT NULL,
    s_i_id INT NOT NULL,
    s_quantity INT NOT NULL,
    s_data TEXT,
    s_dist_01 TEXT,
    s_dist_02 TEXT,
    s_dist_03 TEXT,
    s_dist_04 TEXT,
    s_dist_05 TEXT,
    s_dist_06 TEXT,
    s_dist_07 TEXT,
    s_dist_08 TEXT,
    s_dist_09 TEXT,
    s_dist_10 TEXT,
    PRIMARY KEY (s_w_id, s_i_id)
);

INSERT INTO prep_row_inference VALUES
    (1,1,10,'a','a','a','a','a','a','a','a','a','a','a'),
    (1,2,10,'b','b','b','b','b','b','b','b','b','b','b'),
    (1,3,10,'c','c','c','c','c','c','c','c','c','c','c'),
    (1,4,10,'d','d','d','d','d','d','d','d','d','d','d'),
    (1,5,10,'e','e','e','e','e','e','e','e','e','e','e');

PREPARE prep_row_infer AS
SELECT s_i_id, s_quantity, s_data, s_dist_01, s_dist_02, s_dist_03, s_dist_04,
       s_dist_05, s_dist_06, s_dist_07, s_dist_08, s_dist_09, s_dist_10
FROM prep_row_inference
WHERE (s_w_id, s_i_id) IN (($1,$2),($3,$4),($5,$6),($7,$8),($9,$10))
FOR UPDATE;
EXECUTE prep_row_infer(1,1,1,2,1,3,1,4,1,5);
DEALLOCATE prep_row_infer;

PREPARE prep_row_infer_typed (INT,INT,INT,INT,INT,INT,INT,INT,INT,INT) AS
SELECT s_i_id, s_quantity, s_data, s_dist_01, s_dist_02, s_dist_03, s_dist_04,
       s_dist_05, s_dist_06, s_dist_07, s_dist_08, s_dist_09, s_dist_10
FROM prep_row_inference
WHERE (s_w_id, s_i_id) IN (($1,$2),($3,$4),($5,$6),($7,$8),($9,$10))
FOR UPDATE;
EXECUTE prep_row_infer_typed(1,1,1,2,1,3,1,4,1,5);
DEALLOCATE prep_row_infer_typed;

DROP TABLE prep_row_inference;
