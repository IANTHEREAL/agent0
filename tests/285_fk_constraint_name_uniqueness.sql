-- Issue #1531: CREATE TABLE must reject duplicate FK constraint names.
-- PostgreSQL parity: SQLSTATE 42710 (duplicate_object).

DROP TABLE IF EXISTS fk1531_cross_b;
DROP TABLE IF EXISTS fk1531_cross_a;
DROP TABLE IF EXISTS fk1531_distinct;
DROP TABLE IF EXISTS fk1531_inline_mix;
DROP TABLE IF EXISTS fk1531_case_fold;
DROP TABLE IF EXISTS fk1531_dup_name;
DROP TABLE IF EXISTS fk1531_parent;
DROP TABLE IF EXISTS c_1544_auto;
DROP TABLE IF EXISTS p_1544_auto;

CREATE TABLE fk1531_parent (id INT PRIMARY KEY);

-- T1: duplicate FK constraint names in one CREATE TABLE -> error
CREATE TABLE fk1531_dup_name (
    id INT PRIMARY KEY,
    parent_id1 INT,
    parent_id2 INT,
    CONSTRAINT fk1531_dup FOREIGN KEY (parent_id1) REFERENCES fk1531_parent(id),
    CONSTRAINT fk1531_dup FOREIGN KEY (parent_id2) REFERENCES fk1531_parent(id)
);

-- T2: inline FK auto-name collides with table-level FK name -> error
CREATE TABLE fk1531_inline_mix (
    id INT PRIMARY KEY,
    parent_id INT REFERENCES fk1531_parent(id),
    parent_id2 INT,
    CONSTRAINT fk1531_inline_mix_parent_id_fkey
        FOREIGN KEY (parent_id2) REFERENCES fk1531_parent(id)
);

-- T2b: unquoted FK names are case-insensitive (fold to lowercase) -> error
CREATE TABLE fk1531_case_fold (
    id INT PRIMARY KEY,
    parent_id1 INT,
    parent_id2 INT,
    CONSTRAINT FK_DUP FOREIGN KEY (parent_id1) REFERENCES fk1531_parent(id),
    CONSTRAINT fk_dup FOREIGN KEY (parent_id2) REFERENCES fk1531_parent(id)
);

-- T3: distinct FK names in same table -> success
CREATE TABLE fk1531_distinct (
    id INT PRIMARY KEY,
    parent_id1 INT,
    parent_id2 INT,
    CONSTRAINT fk1531_distinct_fk1 FOREIGN KEY (parent_id1) REFERENCES fk1531_parent(id),
    CONSTRAINT fk1531_distinct_fk2 FOREIGN KEY (parent_id2) REFERENCES fk1531_parent(id)
);

-- T4: same FK name reused across different tables -> success
CREATE TABLE fk1531_cross_a (
    id INT PRIMARY KEY,
    parent_id INT,
    CONSTRAINT fk1531_shared_name FOREIGN KEY (parent_id) REFERENCES fk1531_parent(id)
);

CREATE TABLE fk1531_cross_b (
    id INT PRIMARY KEY,
    parent_id INT,
    CONSTRAINT fk1531_shared_name FOREIGN KEY (parent_id) REFERENCES fk1531_parent(id)
);

SELECT table_name
FROM information_schema.tables
WHERE table_schema = 'public'
  AND table_name IN ('fk1531_distinct', 'fk1531_cross_a', 'fk1531_cross_b')
ORDER BY table_name;

DROP TABLE fk1531_cross_b;
DROP TABLE fk1531_cross_a;
DROP TABLE fk1531_distinct;
DROP TABLE fk1531_parent;

-- T5: quoted FK names preserve case (are case-sensitive) -> success
CREATE TABLE fk1531_parent2 (id INT PRIMARY KEY);
CREATE TABLE fk1531_quoted_case (
    id INT PRIMARY KEY,
    parent_id1 INT,
    parent_id2 INT,
    CONSTRAINT "FK_DUP" FOREIGN KEY (parent_id1) REFERENCES fk1531_parent2(id),
    CONSTRAINT "fk_dup" FOREIGN KEY (parent_id2) REFERENCES fk1531_parent2(id)
);

SELECT table_name
FROM information_schema.tables
WHERE table_schema = 'public'
  AND table_name = 'fk1531_quoted_case'
ORDER BY table_name;

DROP TABLE fk1531_quoted_case;
DROP TABLE fk1531_parent2;

-- T6: auto-generated FK names must uniquify with numeric suffixes
CREATE TABLE p_1544_auto (id INT PRIMARY KEY);
CREATE TABLE c_1544_auto (
    id INT PRIMARY KEY,
    pid INT REFERENCES p_1544_auto(id),
    FOREIGN KEY (pid) REFERENCES p_1544_auto(id)
);

SELECT constraint_name
FROM information_schema.table_constraints
WHERE table_schema = 'public'
  AND table_name = 'c_1544_auto'
  AND constraint_type = 'FOREIGN KEY'
ORDER BY constraint_name;

DROP TABLE c_1544_auto;
DROP TABLE p_1544_auto;
