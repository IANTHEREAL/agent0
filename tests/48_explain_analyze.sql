-- EXPLAIN (ANALYZE) smoke test (SELECT/WITH only).

DROP TABLE IF EXISTS explain_analyze_t;

CREATE TABLE explain_analyze_t (id INT PRIMARY KEY, v TEXT);

INSERT INTO explain_analyze_t(id, v) VALUES
  (1, 'a'),
  (2, 'b'),
  (3, 'c');

EXPLAIN (ANALYZE) SELECT * FROM explain_analyze_t WHERE id >= 2 ORDER BY id;

EXPLAIN (ANALYZE) SELECT 1;

DROP TABLE explain_analyze_t;

