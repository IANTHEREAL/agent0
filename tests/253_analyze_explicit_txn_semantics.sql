-- Regression: explicit transaction semantics for bare ANALYZE must be preserved.
-- If bare ANALYZE accidentally commits an explicit transaction, the INSERT below
-- would persist even after ROLLBACK.

SET client_min_messages = warning;
DROP TABLE IF EXISTS t253_analyze_txn;

CREATE TABLE t253_analyze_txn (id INT PRIMARY KEY);
INSERT INTO t253_analyze_txn VALUES (1);

BEGIN;
ANALYZE;
INSERT INTO t253_analyze_txn VALUES (2);
ROLLBACK;

SELECT 'remaining_rows=' || count(*)::text FROM t253_analyze_txn;

DROP TABLE t253_analyze_txn;
