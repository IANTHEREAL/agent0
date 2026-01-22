-- Test UPDATE on primary key columns (Alembic compatibility)

DROP TABLE IF EXISTS alembic_version CASCADE;

CREATE TABLE alembic_version (
    version_num VARCHAR(32) PRIMARY KEY
);

INSERT INTO alembic_version (version_num) VALUES ('abc123');
SELECT * FROM alembic_version;

UPDATE alembic_version SET version_num = 'def456' WHERE version_num = 'abc123';
SELECT * FROM alembic_version;

UPDATE alembic_version SET version_num = 'ghi789' WHERE version_num = 'def456';
SELECT * FROM alembic_version;

-- Test with composite PK
DROP TABLE IF EXISTS composite_pk_test CASCADE;
CREATE TABLE composite_pk_test (
    a INT,
    b INT,
    c TEXT,
    PRIMARY KEY (a, b)
);

INSERT INTO composite_pk_test VALUES (1, 2, 'original');
SELECT * FROM composite_pk_test ORDER BY a, b;

UPDATE composite_pk_test SET a = 10 WHERE a = 1 AND b = 2;
SELECT * FROM composite_pk_test ORDER BY a, b;

UPDATE composite_pk_test SET b = 20 WHERE a = 10;
SELECT * FROM composite_pk_test ORDER BY a, b;

UPDATE composite_pk_test SET a = 100, b = 200 WHERE a = 10 AND b = 20;
SELECT * FROM composite_pk_test ORDER BY a, b;

-- Test duplicate PK detection
INSERT INTO alembic_version (version_num) VALUES ('existing');
UPDATE alembic_version SET version_num = 'existing' WHERE version_num = 'ghi789';

-- Cleanup
DROP TABLE alembic_version;
DROP TABLE composite_pk_test;
