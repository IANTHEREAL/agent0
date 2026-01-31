-- Regression test for issue #59:
-- Ensure DROP TABLE removes name-keyed comment keys so comments don't resurrect
-- after recreating an object with the same name.

DROP TABLE IF EXISTS t_issue59;

CREATE TABLE t_issue59(id INT PRIMARY KEY);
COMMENT ON TABLE t_issue59 IS 'issue59_table_comment';
COMMENT ON COLUMN t_issue59.id IS 'issue59_column_comment';

SELECT description
FROM pg_catalog.pg_description
WHERE description IN ('issue59_table_comment', 'issue59_column_comment')
ORDER BY description;

DROP TABLE t_issue59;

CREATE TABLE t_issue59(id INT PRIMARY KEY);

SELECT description
FROM pg_catalog.pg_description
WHERE description IN ('issue59_table_comment', 'issue59_column_comment')
ORDER BY description;

DROP TABLE t_issue59;
