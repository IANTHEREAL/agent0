-- Background SQL functions
-- Tests pg_background_launch and pg_background_result

-- Test that pg_background_launch returns a bigint task_id
SELECT pg_background_launch('SELECT 1');
-- The result should be a numeric value (task_id)

-- Test pg_background_result with a non-existent task
SELECT pg_background_result(0);
-- Should return 'not found'
