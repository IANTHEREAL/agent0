-- Regression test for issue #42: planner must not choose a partial index unless implication is sound.
-- Otherwise, IndexScan can miss rows because a partial index only contains entries matching its predicate.

-- Case 1: numeric prefix collision (a = 10 contains "a = 1" as substring).
DROP TABLE IF EXISTS t_partial_index_issue42_prefix;

CREATE TABLE t_partial_index_issue42_prefix(a INT);
INSERT INTO t_partial_index_issue42_prefix VALUES (1), (10);

CREATE INDEX idx_partial_issue42_prefix ON t_partial_index_issue42_prefix(a) WHERE a = 10;

SELECT * FROM t_partial_index_issue42_prefix WHERE a = 1 ORDER BY a;

DROP TABLE t_partial_index_issue42_prefix;

-- Case 2: partial conjunct (predicate "a = 1 AND b = 2" does not imply "a = 1").
DROP TABLE IF EXISTS t_partial_index_issue42_conj;

CREATE TABLE t_partial_index_issue42_conj(a INT, b INT);
INSERT INTO t_partial_index_issue42_conj VALUES (1, 1), (1, 2);

CREATE INDEX idx_partial_issue42_conj ON t_partial_index_issue42_conj(a) WHERE a = 1 AND b = 2;

SELECT * FROM t_partial_index_issue42_conj WHERE a = 1 ORDER BY a, b;

DROP TABLE t_partial_index_issue42_conj;
