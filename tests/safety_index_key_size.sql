-- Safety regression test for index key size guard (#2060).
-- Validates that B-tree indexes on TEXT columns reject oversized keys
-- with a clear error instead of an opaque TiKV KeyTooLarge error.
--
-- Encoding overhead for a non-unique index on single TEXT column + INT PK:
--   38 bytes fixed (db prefix 11 + index header 20 + tags 3 + PK 4)
--   + memcomparable string: 1 byte leading flag + ceil(N/8) * 9 chunk bytes
-- Formula: encoded_key = 39 + ceil(N/8) * 9
-- Max N for encoded_key < 8192: N = 7240 (encoded = 8184)
-- N = 7241 produces encoded = 8193 which is rejected (>= limit).

-- ================================================================
-- Setup
-- ================================================================
CREATE TABLE iks_test (id INT PRIMARY KEY, body TEXT);
CREATE INDEX idx_iks_body ON iks_test (body);

-- ================================================================
-- Test 1: Small value — INSERT succeeds
-- ================================================================
INSERT INTO iks_test VALUES (1, repeat('a', 100));
SELECT id, length(body) FROM iks_test WHERE id = 1;

-- ================================================================
-- Test 2: Large value (~10KB) — INSERT rejected by key size guard
-- ================================================================
INSERT INTO iks_test VALUES (2, repeat('x', 10000));

-- ================================================================
-- Test 3: Boundary value just under limit — INSERT succeeds
-- (7240-byte string encodes to 8183-byte key, under 8192 limit.)
-- ================================================================
INSERT INTO iks_test VALUES (3, repeat('b', 7240));
SELECT id, length(body) FROM iks_test WHERE id = 3;

-- ================================================================
-- Test 4: Boundary value at/over limit — INSERT rejected
-- (7250-byte string encodes to ~8196-byte key, exceeds 8192 limit.)
-- ================================================================
INSERT INTO iks_test VALUES (4, repeat('c', 7250));

-- ================================================================
-- Test 5: Composite index — combined columns exceed limit
-- ================================================================
CREATE TABLE iks_comp (id INT PRIMARY KEY, a TEXT, b TEXT);
CREATE INDEX idx_iks_comp ON iks_comp (a, b);
INSERT INTO iks_comp VALUES (1, repeat('d', 4000), repeat('e', 4000));

-- ================================================================
-- Test 6: CREATE INDEX backfill — rejects existing oversized rows
-- ================================================================
CREATE TABLE iks_backfill (id INT PRIMARY KEY, body TEXT);
INSERT INTO iks_backfill VALUES (1, repeat('f', 10000));
CREATE INDEX idx_iks_backfill ON iks_backfill (body);

-- ================================================================
-- Test 7: Without index, large TEXT insert succeeds (no key issue)
-- ================================================================
CREATE TABLE iks_noindex (id INT PRIMARY KEY, body TEXT);
INSERT INTO iks_noindex VALUES (1, repeat('g', 10000));
SELECT id, length(body) FROM iks_noindex WHERE id = 1;

-- ================================================================
-- Test 8: UPDATE that pushes indexed column over limit — rejected
-- ================================================================
UPDATE iks_test SET body = repeat('u', 10000) WHERE id = 1;
-- Verify original data is unchanged
SELECT id, length(body) FROM iks_test WHERE id = 1;

-- ================================================================
-- Test 9: UNIQUE index — oversized key rejected
-- ================================================================
CREATE TABLE iks_unique (id INT PRIMARY KEY, code TEXT);
CREATE UNIQUE INDEX idx_iks_unique ON iks_unique (code);
INSERT INTO iks_unique VALUES (1, repeat('z', 10000));

-- ================================================================
-- Test 10: Recovery after error — shorter INSERT succeeds
-- ================================================================
INSERT INTO iks_test VALUES (5, repeat('r', 100));
SELECT id, length(body) FROM iks_test WHERE id = 5;

-- ================================================================
-- Test 11: Expression index workaround — large TEXT with prefix index
-- (Validates the HINT advice: left(col, 200) avoids key size issue)
-- ================================================================
CREATE TABLE iks_expr (id INT PRIMARY KEY, body TEXT);
CREATE INDEX idx_iks_expr ON iks_expr (left(body, 200));
INSERT INTO iks_expr VALUES (1, repeat('e', 10000));
SELECT id, length(body) FROM iks_expr WHERE id = 1;

-- ================================================================
-- Cleanup
-- ================================================================
DROP TABLE IF EXISTS iks_test;
DROP TABLE IF EXISTS iks_comp;
DROP TABLE IF EXISTS iks_backfill;
DROP TABLE IF EXISTS iks_noindex;
DROP TABLE IF EXISTS iks_unique;
DROP TABLE IF EXISTS iks_expr;
