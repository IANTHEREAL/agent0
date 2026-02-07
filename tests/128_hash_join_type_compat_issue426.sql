-- Issue #426 regression: hash join must not silently change comparison/coercion semantics
-- for equi-join keys with mismatched types (TEXT vs INT).

DROP TABLE IF EXISTS hj_text;
DROP TABLE IF EXISTS hj_int;

CREATE TABLE hj_text(id TEXT);
CREATE TABLE hj_int(id INT);

INSERT INTO hj_text VALUES ('1');
INSERT INTO hj_int VALUES (1);

SELECT COUNT(*) FROM hj_text JOIN hj_int ON hj_text.id = hj_int.id;
SELECT COUNT(*) FROM hj_text JOIN hj_int ON hj_text.id = (hj_int.id + 0);

DROP TABLE hj_text;
DROP TABLE hj_int;

