-- Issue #2638 regression surface: database activity hooks must not change
-- SQL transaction or COPY semantics while the sink is a no-op by default.

DROP TABLE IF EXISTS activity_surface_2638;
CREATE TABLE activity_surface_2638 (id INT PRIMARY KEY, note TEXT);

-- Explicit rollback after a write must leave no persisted rows.
BEGIN;
INSERT INTO activity_surface_2638 VALUES (1, 'rolled_back');
ROLLBACK;

SELECT id, note FROM activity_surface_2638 ORDER BY id;

-- Savepoint rollback after COPY FROM STDIN must undo the COPY write.
BEGIN;
INSERT INTO activity_surface_2638 VALUES (2, 'kept');
SAVEPOINT sp_activity_2638;
COPY activity_surface_2638 (id, note) FROM STDIN;
3	copy_rolled_back
\.
ROLLBACK TO SAVEPOINT sp_activity_2638;
COMMIT;

SELECT id, note FROM activity_surface_2638 ORDER BY id;

-- Autocommit COPY FROM STDIN must still persist rows.
COPY activity_surface_2638 (id, note) FROM STDIN;
4	copy_committed
\.

SELECT id, note FROM activity_surface_2638 ORDER BY id;

-- COPY TO STDOUT is a read path; it should return rows without mutation.
COPY activity_surface_2638 (id, note) TO STDOUT;

SELECT COUNT(*) AS row_count FROM activity_surface_2638;

DROP TABLE activity_surface_2638;
