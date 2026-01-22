-- PostgreSQL compatible tests from database
-- 96 tests

-- Test 1: statement (line 1)
SET client_min_messages = warning;

DROP ROLE IF EXISTS testuser2;
DROP ROLE IF EXISTS testuser;
DROP ROLE IF EXISTS fake_user;
DROP ROLE IF EXISTS newrole;

CREATE ROLE testuser LOGIN;
CREATE ROLE fake_user LOGIN;

DROP DATABASE IF EXISTS a;
CREATE DATABASE a;

-- Test 2: statement (line 4)
DROP DATABASE IF EXISTS a;
CREATE DATABASE a;

-- Test 3: statement (line 7)
DROP DATABASE IF EXISTS a;
CREATE DATABASE a;

-- Test 4: statement (line 10)
DROP DATABASE IF EXISTS empty_name_db;
CREATE DATABASE empty_name_db;

-- Test 5: query (line 13)
SELECT datname
FROM pg_database
WHERE datistemplate = false
ORDER BY datname;

-- Test 6: statement (line 23)
CREATE ROLE newrole LOGIN;

-- Test 7: query (line 26)
SELECT has_database_privilege('newrole', 'a', 'CONNECT');

-- Test 8: statement (line 31)
COMMENT ON DATABASE a IS 'A';

-- Test 9: query (line 34)
SELECT d.datname, obj_description(d.oid, 'pg_database') AS comment
FROM pg_database AS d
WHERE d.datistemplate = false
ORDER BY d.datname;

-- Test 10: statement (line 44)
COMMENT ON DATABASE a IS 'AAA';

-- Test 11: query (line 47)
SELECT d.datname, obj_description(d.oid, 'pg_database') AS comment
FROM pg_database AS d
WHERE d.datistemplate = false
ORDER BY d.datname;

-- Test 12: statement (line 58)
\connect a
CREATE SCHEMA s;

-- Test 13: query (line 61)
SELECT schema_name
FROM information_schema.schemata
ORDER BY schema_name;

-- Test 14: statement (line 72)
\connect postgres
DROP DATABASE IF EXISTS b;
CREATE DATABASE b TEMPLATE=template0;

-- Test 15: statement (line 75)
DROP DATABASE IF EXISTS c;
CREATE DATABASE c TEMPLATE=template0;

-- Test 16: statement (line 78)
DROP DATABASE IF EXISTS c;
CREATE DATABASE c TEMPLATE=template0;

-- Test 17: statement (line 81)
DROP DATABASE IF EXISTS b2;
CREATE DATABASE b2 ENCODING='UTF8';

-- Test 18: statement (line 84)
DROP DATABASE IF EXISTS c;
CREATE DATABASE c ENCODING='UTF8';

-- Test 19: statement (line 87)
DROP DATABASE IF EXISTS c;
CREATE DATABASE c ENCODING='UTF8';

-- Test 20: statement (line 90)
DROP DATABASE IF EXISTS b3;
CREATE DATABASE b3 TEMPLATE=template0 LC_COLLATE='C' LC_CTYPE='C';

-- Test 21: statement (line 93)
DROP DATABASE IF EXISTS c;
CREATE DATABASE c TEMPLATE=template0 LC_COLLATE='C' LC_CTYPE='C';

-- Test 22: statement (line 96)
DROP DATABASE IF EXISTS c;
CREATE DATABASE c TEMPLATE=template0 LC_COLLATE='C' LC_CTYPE='C';

-- Test 23: statement (line 99)
DROP DATABASE IF EXISTS b4;
CREATE DATABASE b4 TEMPLATE=template0 LC_CTYPE='C' LC_COLLATE='C';

-- Test 24: statement (line 102)
DROP DATABASE IF EXISTS c;
CREATE DATABASE c TEMPLATE=template0 LC_CTYPE='C' LC_COLLATE='C';

-- Test 25: statement (line 105)
DROP DATABASE IF EXISTS c;
CREATE DATABASE c TEMPLATE=template0 LC_CTYPE='C' LC_COLLATE='C';

-- Test 26: statement (line 108)
DROP DATABASE IF EXISTS b5;
CREATE DATABASE b5 TEMPLATE=template0 ENCODING='UTF8' LC_COLLATE='C' LC_CTYPE='C';

-- Test 27: statement (line 111)
DROP DATABASE IF EXISTS b6;
CREATE DATABASE b6 TEMPLATE template0 ENCODING 'UTF8' LC_COLLATE 'C' LC_CTYPE 'C';

-- Test 28: statement (line 114)
DROP DATABASE IF EXISTS b7;
CREATE DATABASE b7 WITH CONNECTION LIMIT -1;

-- Test 29: statement (line 117)
DROP DATABASE IF EXISTS b8;
CREATE DATABASE b8 WITH CONNECTION LIMIT = 1;

-- Test 30: statement (line 120)
DROP DATABASE IF EXISTS c;
CREATE DATABASE c;

-- Test 31: query (line 123)
SELECT datname
FROM pg_database
WHERE datistemplate = false
ORDER BY datname;

-- Test 32: statement (line 140)
\connect b
CREATE TABLE a (id INT PRIMARY KEY);

-- Test 33: statement (line 143)
INSERT INTO a VALUES (3),(7),(2);

-- Test 34: query (line 146)
SELECT * FROM a;

-- Test 35: statement (line 153)
\connect postgres
SELECT 1;

-- Test 36: statement (line 156)
SELECT 1;

-- Test 37: statement (line 159)
\connect b
SELECT * FROM a;

-- Test 38: statement (line 162)
\connect postgres
DROP DATABASE IF EXISTS b;

-- Test 39: statement (line 165)
DROP DATABASE IF EXISTS b;

-- Test 40: statement (line 168)
DROP DATABASE IF EXISTS b2;

-- Test 41: statement (line 171)
DROP DATABASE IF EXISTS b3;

-- Test 42: statement (line 174)
DROP DATABASE IF EXISTS b4;

-- Test 43: statement (line 177)
DROP DATABASE IF EXISTS b5;

-- Test 44: statement (line 180)
DROP DATABASE IF EXISTS b6;

-- Test 45: statement (line 183)
DROP DATABASE IF EXISTS b7;

-- Test 46: statement (line 186)
DROP DATABASE IF EXISTS empty_name_db;

-- Test 47: query (line 189)
SELECT datname
FROM pg_database
WHERE datistemplate = false
ORDER BY datname;

-- Test 48: statement (line 200)
DROP DATABASE IF EXISTS b;
CREATE DATABASE b;

-- Test 49: statement (line 203)
\connect b
SELECT 1;

-- Test 50: statement (line 206)
CREATE TABLE a (id INT PRIMARY KEY);

-- Test 51: query (line 209)
SELECT * FROM a;

-- Test 52: statement (line 216)
DROP DATABASE IF EXISTS privs;
CREATE DATABASE privs;

-- user root

-- Test 53: statement (line 221)
ALTER USER testuser CREATEDB;

-- user testuser

-- Test 54: statement (line 226)
DROP DATABASE IF EXISTS privs;
CREATE DATABASE privs;

-- Test 55: statement (line 230)
DROP DATABASE IF EXISTS privs;

-- Test 56: statement (line 234)
DROP DATABASE IF EXISTS a;
CREATE DATABASE a;

-- user root

-- Test 57: statement (line 239)
ALTER DATABASE a OWNER TO testuser;

-- user testuser

-- Test 58: statement (line 244)
DROP DATABASE IF EXISTS a;

-- Test 59: statement (line 250)
ALTER USER testuser NOCREATEDB;

-- user testuser

-- Test 60: statement (line 255)
DROP DATABASE IF EXISTS privs;
CREATE DATABASE privs;

-- user root

-- Test 61: statement (line 262)
DROP DATABASE IF EXISTS db1;
CREATE DATABASE db1;

-- Test 62: statement (line 265)
\connect db1

-- Test 63: statement (line 268)
-- SET sql_safe_updates=false;

-- Test 64: statement (line 271)
\connect postgres
DROP DATABASE IF EXISTS db1;

-- Test 65: statement (line 274)
SELECT * FROM pg_settings;

-- Test 66: statement (line 277)
SELECT * FROM pg_catalog.pg_attribute;

-- Test 67: statement (line 280)
SELECT NULL;

-- Test 68: statement (line 285)
DROP DATABASE IF EXISTS db69713;
CREATE DATABASE db69713;

-- Test 69: statement (line 288)
\connect db69713

-- Test 70: statement (line 291)
CREATE SCHEMA s;

-- Test 71: statement (line 294)
CREATE TYPE s.testenum AS ENUM ('foo', 'bar', 'baz');

-- Test 72: statement (line 297)
CREATE TABLE s.pg_indexdef_test (
    a INT PRIMARY KEY,
    e s.testenum
);

-- Test 73: statement (line 303)
CREATE TABLE s.pg_constraintdef_test (
  a int,
  FOREIGN KEY(a) REFERENCES s.pg_indexdef_test(a) ON DELETE CASCADE
);

-- Test 74: statement (line 309)
\connect postgres
DROP DATABASE IF EXISTS db69713;

-- Test 75: statement (line 312)
\connect postgres

-- Test 76: statement (line 318)
DROP DATABASE IF EXISTS aa;
CREATE DATABASE aa WITH OWNER fake_user;

-- Test 77: statement (line 321)
DROP DATABASE IF EXISTS a;
CREATE DATABASE a WITH OWNER testuser;

-- Test 78: query (line 324)
SELECT datname
FROM pg_database
WHERE datistemplate = false
ORDER BY datname;

-- Test 79: statement (line 337)
DROP ROLE IF EXISTS testuser2;
CREATE USER testuser2;

-- user testuser

-- Test 80: statement (line 342)
DROP DATABASE IF EXISTS d;
CREATE DATABASE d WITH OWNER testuser2;

-- user root

-- Test 81: statement (line 350)
DROP DATABASE IF EXISTS ifnotexistsownerdb;
CREATE DATABASE ifnotexistsownerdb WITH OWNER testuser;

-- Test 82: query (line 353)
SELECT datname
FROM pg_database
WHERE datname = 'ifnotexistsownerdb';

-- Test 83: statement (line 359)
DROP DATABASE IF EXISTS ifnotexistsownerdb;
CREATE DATABASE ifnotexistsownerdb WITH OWNER testuser;

-- Test 84: query (line 362)
SELECT datname
FROM pg_database
WHERE datname = 'ifnotexistsownerdb';

-- Test 85: statement (line 372)
CREATE SCHEMA regression_105906;

-- Test 86: statement (line 375)
DROP DATABASE IF EXISTS regression_105906;
CREATE DATABASE regression_105906;

-- Test 87: query (line 378)
SELECT schema_name
FROM information_schema.schemata
ORDER BY schema_name;

-- Test 88: query (line 390)
\connect regression_105906
SELECT schema_name
FROM information_schema.schemata
ORDER BY schema_name;

-- Test 89: statement (line 400)
\connect postgres
DROP DATABASE IF EXISTS regression_105906;

-- Test 90: statement (line 403)
DROP SCHEMA IF EXISTS regression_105906;

-- Test 91: statement (line 406)
CREATE SCHEMA "rEgReSsIoN 105906";

-- Test 92: statement (line 409)
DROP DATABASE IF EXISTS "rEgReSsIoN 105906";
CREATE DATABASE "rEgReSsIoN 105906";

-- Test 93: query (line 412)
SELECT schema_name
FROM information_schema.schemata
ORDER BY schema_name;

-- Test 94: query (line 423)
\connect "rEgReSsIoN 105906"
SELECT schema_name
FROM information_schema.schemata
ORDER BY schema_name;

-- Test 95: statement (line 432)
\connect postgres
DROP SCHEMA IF EXISTS "rEgReSsIoN 105906";

-- Test 96: statement (line 435)
DROP DATABASE IF EXISTS "rEgReSsIoN 105906";

RESET client_min_messages;
