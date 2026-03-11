SET client_min_messages = warning;
DROP TABLE IF EXISTS t_copy_esc;
CREATE TABLE t_copy_esc (id INT, name TEXT);

-- 1. ESCAPE-only, default (text) format -> error
COPY t_copy_esc FROM STDIN WITH (ESCAPE '\');

-- 2. ESCAPE-only, explicit FORMAT text -> error
COPY t_copy_esc FROM STDIN WITH (FORMAT text, ESCAPE '\');

-- 3. CSV with ESCAPE still works (positive control)
COPY t_copy_esc FROM STDIN WITH (FORMAT csv, ESCAPE '\');
1,"hello"
\.

SELECT * FROM t_copy_esc ORDER BY id;
DROP TABLE t_copy_esc;
