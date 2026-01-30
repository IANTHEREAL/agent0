-- Regression test for issue #51: tables without explicit PK must still have a stable
-- physical row identifier for secondary indexes.

DROP TABLE IF EXISTS pkless_idx;
CREATE TABLE pkless_idx(a INT);
CREATE INDEX idx_pkless_idx_a ON pkless_idx(a);

INSERT INTO pkless_idx VALUES (1);

-- Full scan should see the row.
SELECT * FROM pkless_idx;

-- Index path should also see the row (planner should choose an index scan here).
SELECT * FROM pkless_idx WHERE a = 1;

DROP TABLE pkless_idx;

