-- DB9 cop pushdown: EXPLAIN VERBOSE keeps PG node names and exposes TiKV annotations.

DROP TABLE IF EXISTS db9_cop_verbose_smoke;
CREATE TABLE db9_cop_verbose_smoke(id INT PRIMARY KEY, v TEXT, n INT);
CREATE INDEX db9_cop_verbose_smoke_n_idx ON db9_cop_verbose_smoke(n);
INSERT INTO db9_cop_verbose_smoke VALUES (1, 'a', 10), (2, 'b', 20), (3, 'c', 30);

SET db9.enable_cop_pushdown = on;

EXPLAIN VERBOSE
SELECT lower(v)
FROM db9_cop_verbose_smoke
WHERE n = 20
LIMIT 1;

EXPLAIN VERBOSE
SELECT id
FROM db9_cop_verbose_smoke
WHERE lower(v) = 'b'
LIMIT 1;

DROP TABLE db9_cop_verbose_smoke;
