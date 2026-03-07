SET client_min_messages = warning;
DROP TABLE IF EXISTS t_copy_qe;
CREATE TABLE t_copy_qe (id INT, name TEXT);

-- 1. Non-CSV QUOTE (modern syntax) -> error
COPY t_copy_qe FROM STDIN WITH (QUOTE '"');

-- 2. Explicit FORMAT text with QUOTE -> error
COPY t_copy_qe FROM STDIN WITH (FORMAT text, QUOTE '"');

-- 3. QUOTE > ESCAPE precedence -> QUOTE error
COPY t_copy_qe FROM STDIN WITH (QUOTE '"', ESCAPE '\');

-- 4. Explicit FORMAT text with QUOTE+ESCAPE -> QUOTE error
COPY t_copy_qe FROM STDIN WITH (FORMAT text, QUOTE '"', ESCAPE '\');

-- 5. CSV QUOTE still works
COPY t_copy_qe FROM STDIN WITH (FORMAT csv, QUOTE E'\'');
1,'hello'
\.

-- 6. CSV FORCE NOT NULL still works
COPY t_copy_qe FROM STDIN CSV HEADER FORCE NOT NULL name;
id,name
2,something
\.

SELECT * FROM t_copy_qe ORDER BY id;
DROP TABLE t_copy_qe;
