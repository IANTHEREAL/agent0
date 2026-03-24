-- Regression guard: COPY FORMAT binary must be explicitly rejected.

DROP TABLE IF EXISTS copy_bin_test;
CREATE TABLE copy_bin_test (id INT PRIMARY KEY, val TEXT);

-- Must error with "FORMAT binary is not supported"
COPY copy_bin_test FROM STDIN WITH (FORMAT binary);

DROP TABLE copy_bin_test;
