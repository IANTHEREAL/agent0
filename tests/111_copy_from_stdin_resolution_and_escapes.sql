DROP TABLE IF EXISTS public.t_copy_resolve;
DROP SCHEMA IF EXISTS copy_schema CASCADE;
CREATE SCHEMA copy_schema;

CREATE TABLE copy_schema.t_copy_resolve (id INT, note TEXT);
CREATE TABLE public.t_copy_resolve (id INT, note TEXT);

SET search_path TO copy_schema, public;

COPY t_copy_resolve (ID, NOTE) FROM STDIN;
1	\\t
2	\\n
3	\\r
4	\t
5	\n
6	\r
\.

SELECT COUNT(*) FROM copy_schema.t_copy_resolve;
SELECT COUNT(*) FROM public.t_copy_resolve;

SELECT id,
       octet_length(note) AS len,
       ascii(note) AS a1,
       ascii(SUBSTRING(note FROM 2 FOR 1)) AS a2
FROM copy_schema.t_copy_resolve
ORDER BY id;

COPY public.t_copy_resolve (id, note) FROM STDIN;
10	ok
\.

SELECT COUNT(*) FROM public.t_copy_resolve;

COPY t_copy_resolve (id, badcol) FROM STDIN;
