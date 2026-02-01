DROP TABLE IF EXISTS public.t_copy_search_path;
DROP SCHEMA IF EXISTS copy_schema1 CASCADE;
DROP SCHEMA IF EXISTS copy_schema2 CASCADE;
CREATE SCHEMA copy_schema1;
CREATE SCHEMA copy_schema2;

CREATE TABLE copy_schema2.t_copy_search_path (id INT);

SET search_path TO copy_schema1, copy_schema2, public;

COPY t_copy_search_path (id) FROM STDIN;
1
\.

SELECT COUNT(*) FROM copy_schema2.t_copy_search_path;
