SET client_min_messages = warning;
DROP TABLE IF EXISTS t_copy_nonascii;
CREATE TABLE t_copy_nonascii (id INT, name TEXT);

-- 1. Non-ASCII QUOTE in modern syntax -> semantic error (0A000), not parser error (42601)
COPY t_copy_nonascii FROM STDIN WITH (FORMAT CSV, QUOTE 'é');

-- 2. Non-ASCII ESCAPE in modern syntax -> semantic error
COPY t_copy_nonascii FROM STDIN WITH (FORMAT CSV, ESCAPE 'ñ');

-- 3. Non-ASCII DELIMITER in modern syntax -> semantic error
COPY t_copy_nonascii FROM STDIN WITH (DELIMITER '€');

DROP TABLE t_copy_nonascii;
