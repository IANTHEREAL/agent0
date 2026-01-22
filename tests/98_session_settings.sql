-- Session settings (GUC) smoke test: SHOW + set_config(search_path)

CREATE SCHEMA ss_s1;
CREATE SCHEMA ss_s2;

CREATE TABLE ss_s1.t (id INT PRIMARY KEY, v TEXT);
CREATE TABLE ss_s2.t (id INT PRIMARY KEY, v TEXT);

INSERT INTO ss_s1.t VALUES (1, 'a');
INSERT INTO ss_s2.t VALUES (1, 'b');

-- set_config updates search_path and returns the previous value
SELECT pg_catalog.set_config('search_path', 'ss_s1', false) AS prev;
SHOW search_path;
SELECT v FROM t ORDER BY id;

SELECT set_config('search_path', 'ss_s2', false) AS prev;
SHOW search_path;
SELECT v FROM t ORDER BY id;

-- Store/read back a few common pg_dump session variables
SET statement_timeout = 0;
SHOW statement_timeout;
SET client_encoding = 'UTF8';
SHOW client_encoding;

-- Cleanup
DROP TABLE ss_s1.t;
DROP TABLE ss_s2.t;
DROP SCHEMA ss_s1;
DROP SCHEMA ss_s2;
