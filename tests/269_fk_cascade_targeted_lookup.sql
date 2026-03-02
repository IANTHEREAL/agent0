-- FK cascade targeted lookup regression test (#1299)
-- Verifies cascade DELETE and UPDATE correctness after replacing
-- full-table preload with lazy per-PK targeted lookup.

-- Scenario 1: Delete parent with 0 children (10K children reference other parent)
CREATE TABLE fk_cascade_parent (id INT PRIMARY KEY);
CREATE TABLE fk_cascade_child (
    id SERIAL PRIMARY KEY,
    parent_id INT REFERENCES fk_cascade_parent(id) ON DELETE CASCADE
);
CREATE INDEX idx_child_parent_id ON fk_cascade_child(parent_id);

INSERT INTO fk_cascade_parent VALUES (1), (2);
INSERT INTO fk_cascade_child (parent_id)
    SELECT 1 FROM generate_series(1, 100);

DELETE FROM fk_cascade_parent WHERE id = 2;
SELECT COUNT(*) FROM fk_cascade_parent;
SELECT COUNT(*) FROM fk_cascade_child;

-- Scenario 2: Delete parent with children
DELETE FROM fk_cascade_parent WHERE id = 1;
SELECT COUNT(*) FROM fk_cascade_parent;
SELECT COUNT(*) FROM fk_cascade_child;

-- Scenario 3: Multi-level cascade with indexes
CREATE TABLE ml_gp (id INT PRIMARY KEY);
CREATE TABLE ml_p (
    id INT PRIMARY KEY,
    gp_id INT REFERENCES ml_gp(id) ON DELETE CASCADE
);
CREATE TABLE ml_c (
    id INT PRIMARY KEY,
    p_id INT REFERENCES ml_p(id) ON DELETE CASCADE
);
CREATE INDEX idx_mlp_gp ON ml_p(gp_id);
CREATE INDEX idx_mlc_p ON ml_c(p_id);

INSERT INTO ml_gp VALUES (1);
INSERT INTO ml_p VALUES (10, 1), (20, 1);
INSERT INTO ml_c VALUES (100, 10), (200, 10), (300, 20);

DELETE FROM ml_gp WHERE id = 1;
SELECT COUNT(*) FROM ml_gp;
SELECT COUNT(*) FROM ml_p;
SELECT COUNT(*) FROM ml_c;

-- Scenario 4: Non-indexed FK (batched filter fallback)
CREATE TABLE ni_parent (id INT PRIMARY KEY);
CREATE TABLE ni_child (
    id SERIAL PRIMARY KEY,
    parent_id INT REFERENCES ni_parent(id) ON DELETE CASCADE
);
-- No index on parent_id — forces batched-filter fallback

INSERT INTO ni_parent VALUES (1), (2);
INSERT INTO ni_child (parent_id) VALUES (1), (1), (2);

DELETE FROM ni_parent WHERE id = 1;
SELECT COUNT(*) FROM ni_parent;
SELECT COUNT(*) FROM ni_child;

-- Scenario 5: ON UPDATE CASCADE
CREATE TABLE uc_parent (id INT PRIMARY KEY);
CREATE TABLE uc_child (
    id SERIAL PRIMARY KEY,
    parent_id INT REFERENCES uc_parent(id) ON UPDATE CASCADE
);
CREATE INDEX idx_uc_child ON uc_child(parent_id);

INSERT INTO uc_parent VALUES (1);
INSERT INTO uc_child (parent_id) VALUES (1), (1);

UPDATE uc_parent SET id = 10 WHERE id = 1;
SELECT COUNT(*) FROM uc_child WHERE parent_id = 10;

-- Cleanup
DROP TABLE fk_cascade_child, fk_cascade_parent;
DROP TABLE ml_c, ml_p, ml_gp;
DROP TABLE ni_child, ni_parent;
DROP TABLE uc_child, uc_parent;
