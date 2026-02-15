-- Contract tests for structural fixes:
--   P0-1: metadata freshness (no TTL cache)
--   P0-2: view resolution via Analyzer prefetch
--   P0-3: vector text encoding consistency

-- =========================================================
-- P0-3: Vector wire roundtrip — integers must not have .0
-- =========================================================

DROP TABLE IF EXISTS vec_roundtrip;
CREATE TABLE vec_roundtrip (id INT PRIMARY KEY, v vector(3));

INSERT INTO vec_roundtrip VALUES (1, '[1,2,3]');
INSERT INTO vec_roundtrip VALUES (2, '[1.5,2,3.7]');
INSERT INTO vec_roundtrip VALUES (3, '[]');

-- Wire encoding: integers must output as 1,2,3 not 1.0,2.0,3.0
SELECT v FROM vec_roundtrip ORDER BY id;

-- CAST roundtrip
SELECT CAST('[10,20,30]' AS vector) AS v;

DROP TABLE vec_roundtrip;

-- =========================================================
-- P0-2: CREATE VIEW then SELECT through Analyzer prefetch
-- =========================================================

DROP VIEW IF EXISTS v_contract_filtered;
DROP VIEW IF EXISTS v_contract_all;
DROP TABLE IF EXISTS t_contract;

CREATE TABLE t_contract (id INT PRIMARY KEY, val TEXT);
INSERT INTO t_contract VALUES (1, 'a'), (2, 'b'), (3, 'c');

-- Simple view
CREATE VIEW v_contract_all AS SELECT * FROM t_contract;
SELECT * FROM v_contract_all ORDER BY id;

-- View with WHERE
CREATE VIEW v_contract_filtered AS SELECT * FROM t_contract WHERE id > 1;
SELECT * FROM v_contract_filtered ORDER BY id;

-- View with additional WHERE at query time
SELECT * FROM v_contract_all WHERE val = 'b';

-- View in JOIN
SELECT a.id, f.val
FROM v_contract_all a
JOIN v_contract_filtered f ON a.id = f.id
ORDER BY a.id;

DROP VIEW v_contract_filtered;
DROP VIEW v_contract_all;
DROP TABLE t_contract;

-- =========================================================
-- P0-1: Metadata freshness — no stale cache between queries
-- =========================================================

DROP TABLE IF EXISTS t_fresh;
CREATE TABLE t_fresh (x INT PRIMARY KEY);

-- Table should be immediately visible in information_schema
SELECT table_name FROM information_schema.tables
WHERE table_schema = 'public' AND table_name = 't_fresh';

DROP TABLE t_fresh;

-- After DROP, table should be immediately gone
SELECT table_name FROM information_schema.tables
WHERE table_schema = 'public' AND table_name = 't_fresh';
