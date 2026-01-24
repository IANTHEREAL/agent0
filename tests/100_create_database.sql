-- CREATE/DROP/ALTER DATABASE support (storage format v2)

DROP DATABASE IF EXISTS createdb_100_renamed;
DROP DATABASE IF EXISTS createdb_100;

CREATE DATABASE createdb_100 WITH OWNER = admin;
SELECT datname FROM pg_catalog.pg_database WHERE datname = 'createdb_100' ORDER BY datname;

\connect createdb_100
SELECT current_database();
CREATE TABLE public.dbtest (id INT PRIMARY KEY, v TEXT);
INSERT INTO public.dbtest (id, v) VALUES (1, 'a'), (2, 'b');
SELECT id, v FROM public.dbtest ORDER BY id;

\connect postgres
ALTER DATABASE createdb_100 RENAME TO createdb_100_renamed;
SELECT datname FROM pg_catalog.pg_database WHERE datname LIKE 'createdb_100%' ORDER BY datname;

\connect createdb_100_renamed
SELECT current_database();
SELECT id, v FROM public.dbtest ORDER BY id;

\connect postgres
DROP DATABASE createdb_100_renamed;
SELECT datname FROM pg_catalog.pg_database WHERE datname LIKE 'createdb_100%' ORDER BY datname;

