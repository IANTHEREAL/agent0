-- Auto-ANALYZE threshold test
-- Tests DML operations that should trigger auto-ANALYZE threshold

DROP TABLE IF EXISTS analyze_test;
CREATE TABLE analyze_test (id INTEGER PRIMARY KEY, val TEXT);

-- Insert enough rows to trigger threshold check
INSERT INTO analyze_test SELECT g, 'value_' || g FROM generate_series(1, 100) AS g;

-- Verify data is inserted
SELECT count(*) FROM analyze_test;

-- Manual ANALYZE should work
ANALYZE analyze_test;

-- Clean up
DROP TABLE analyze_test;
