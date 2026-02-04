-- ported from pg_tests PR#58: compatible/alter_primary_key.sql
--
-- Alter the PRIMARY KEY by dropping and re-adding it on different columns.
-- Verify the active PK columns via information_schema.

SET client_min_messages = warning;

-- Cleanup from prior runs.
DROP TABLE IF EXISTS t145_alter_primary_key;

CREATE TABLE t145_alter_primary_key (
  x INT PRIMARY KEY,
  y INT NOT NULL,
  z INT NOT NULL,
  w INT
);

ALTER TABLE t145_alter_primary_key DROP CONSTRAINT t145_alter_primary_key_pkey;
ALTER TABLE t145_alter_primary_key ADD PRIMARY KEY (y, z);

SELECT kcu.column_name, kcu.ordinal_position
FROM information_schema.table_constraints tc
JOIN information_schema.key_column_usage kcu
  ON tc.constraint_name = kcu.constraint_name
 AND tc.table_schema = kcu.table_schema
 AND tc.table_name = kcu.table_name
WHERE tc.table_schema = 'public'
  AND tc.table_name = 't145_alter_primary_key'
  AND tc.constraint_type = 'PRIMARY KEY'
ORDER BY kcu.ordinal_position;

-- With the PK moved off of x, duplicate x values should now be allowed.
INSERT INTO t145_alter_primary_key VALUES
  (1, 2, 3, 4),
  (1, 6, 7, 8),
  (9, 10, 11, 12);

SELECT x, y, z, w
FROM t145_alter_primary_key
ORDER BY y, z;

DROP TABLE t145_alter_primary_key;
