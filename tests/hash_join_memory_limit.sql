-- Test: hash join memory limit enforcement (db9.hash_join_work_mem)
-- Set a very small limit so the join triggers the enforcement.
SET db9.hash_join_work_mem = '1KB';

CREATE TABLE hjm_left (id INT PRIMARY KEY, val TEXT);
CREATE TABLE hjm_right (id INT PRIMARY KEY, val TEXT);

INSERT INTO hjm_left SELECT g, repeat('x', 100) FROM generate_series(1, 50) g;
INSERT INTO hjm_right SELECT g, repeat('y', 100) FROM generate_series(1, 50) g;

-- This join should fail: 50 rows * ~100 bytes each > 1KB
SELECT count(*) FROM hjm_left JOIN hjm_right ON hjm_left.id = hjm_right.id;

DROP TABLE hjm_left;
DROP TABLE hjm_right;
RESET db9.hash_join_work_mem;
