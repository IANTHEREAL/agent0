-- PostgreSQL compatible tests from pg_catalog_pg_default_acl_with_grant_option
-- 65 tests

SET client_min_messages = warning;

-- Capture the current role name for psql identifier substitution (see :\"cur_user\").
SELECT current_user AS cur_user \gset

DROP ROLE IF EXISTS crdb_tests_bar;
DROP ROLE IF EXISTS crdb_tests_public;
DROP ROLE IF EXISTS crdb_tests_foo;

-- Test 1: statement (line 1)
CREATE ROLE crdb_tests_public;
ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO crdb_tests_public WITH GRANT OPTION;

-- Test 2: statement (line 4)
ALTER DEFAULT PRIVILEGES GRANT USAGE ON SCHEMAS TO crdb_tests_public WITH GRANT OPTION;

-- Test 3: statement (line 7)
ALTER DEFAULT PRIVILEGES GRANT SELECT ON SEQUENCES TO crdb_tests_public WITH GRANT OPTION;

-- Test 4: statement (line 10)
ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO PUBLIC;

-- Test 5: statement (line 13)
ALTER DEFAULT PRIVILEGES GRANT USAGE ON SCHEMAS TO PUBLIC;

-- Test 6: statement (line 16)
ALTER DEFAULT PRIVILEGES GRANT SELECT ON SEQUENCES TO PUBLIC;

-- Test 7: query (line 20)
SELECT
  r.rolname AS defaclrole,
  COALESCE(n.nspname, '<global>') AS defaclnamespace,
  d.defaclobjtype,
  d.defaclacl
FROM pg_catalog.pg_default_acl AS d
JOIN pg_catalog.pg_roles AS r ON r.oid = d.defaclrole
LEFT JOIN pg_catalog.pg_namespace AS n ON n.oid = d.defaclnamespace
ORDER BY 1, 2, 3;

-- Test 8: statement (line 28)
CREATE ROLE crdb_tests_foo;

-- Test 9: statement (line 31)
CREATE ROLE crdb_tests_bar;

-- Test 10: statement (line 34)
ALTER DEFAULT PRIVILEGES GRANT ALL ON TABLES TO crdb_tests_foo, crdb_tests_bar WITH GRANT OPTION;

-- Test 11: statement (line 37)
ALTER DEFAULT PRIVILEGES GRANT ALL ON TYPES TO crdb_tests_foo, crdb_tests_bar WITH GRANT OPTION;

-- Test 12: statement (line 40)
ALTER DEFAULT PRIVILEGES GRANT ALL ON SCHEMAS TO crdb_tests_foo, crdb_tests_bar WITH GRANT OPTION;

-- Test 13: statement (line 43)
ALTER DEFAULT PRIVILEGES GRANT ALL ON SEQUENCES TO crdb_tests_foo, crdb_tests_bar WITH GRANT OPTION;

-- Test 14: statement (line 46)
ALTER DEFAULT PRIVILEGES GRANT ALL ON FUNCTIONS TO crdb_tests_foo, crdb_tests_bar WITH GRANT OPTION;

-- Test 15: query (line 49)
SELECT
  r.rolname AS defaclrole,
  COALESCE(n.nspname, '<global>') AS defaclnamespace,
  d.defaclobjtype,
  d.defaclacl
FROM pg_catalog.pg_default_acl AS d
JOIN pg_catalog.pg_roles AS r ON r.oid = d.defaclrole
LEFT JOIN pg_catalog.pg_namespace AS n ON n.oid = d.defaclnamespace
ORDER BY 1, 2, 3;

-- Test 16: statement (line 59)
ALTER DEFAULT PRIVILEGES REVOKE GRANT OPTION FOR SELECT, DELETE ON TABLES FROM crdb_tests_foo, crdb_tests_bar;

-- Test 17: statement (line 62)
ALTER DEFAULT PRIVILEGES REVOKE GRANT OPTION FOR USAGE ON TYPES FROM crdb_tests_foo, crdb_tests_bar;

-- Test 18: statement (line 65)
ALTER DEFAULT PRIVILEGES REVOKE GRANT OPTION FOR CREATE ON SCHEMAS FROM crdb_tests_foo, crdb_tests_bar;

-- Test 19: statement (line 68)
ALTER DEFAULT PRIVILEGES REVOKE GRANT OPTION FOR USAGE, UPDATE ON SEQUENCES FROM crdb_tests_foo, crdb_tests_bar;

-- Test 20: statement (line 71)
ALTER DEFAULT PRIVILEGES REVOKE GRANT OPTION FOR EXECUTE ON FUNCTIONS FROM crdb_tests_foo, crdb_tests_bar;

-- Test 21: query (line 74)
SELECT
  r.rolname AS defaclrole,
  COALESCE(n.nspname, '<global>') AS defaclnamespace,
  d.defaclobjtype,
  d.defaclacl
FROM pg_catalog.pg_default_acl AS d
JOIN pg_catalog.pg_roles AS r ON r.oid = d.defaclrole
LEFT JOIN pg_catalog.pg_namespace AS n ON n.oid = d.defaclnamespace
ORDER BY 1, 2, 3;

-- Test 22: statement (line 84)
ALTER DEFAULT PRIVILEGES REVOKE GRANT OPTION FOR ALL ON TABLES FROM crdb_tests_foo, crdb_tests_bar;

-- Test 23: statement (line 87)
ALTER DEFAULT PRIVILEGES REVOKE GRANT OPTION FOR ALL ON TYPES FROM crdb_tests_foo, crdb_tests_bar;

-- Test 24: statement (line 90)
ALTER DEFAULT PRIVILEGES REVOKE GRANT OPTION FOR ALL ON SCHEMAS FROM crdb_tests_foo, crdb_tests_bar;

-- Test 25: statement (line 93)
ALTER DEFAULT PRIVILEGES REVOKE GRANT OPTION FOR ALL ON SEQUENCES FROM crdb_tests_foo, crdb_tests_bar;

-- Test 26: statement (line 96)
ALTER DEFAULT PRIVILEGES REVOKE GRANT OPTION FOR ALL ON FUNCTIONS FROM crdb_tests_foo, crdb_tests_bar;

-- Test 27: query (line 99)
SELECT
  r.rolname AS defaclrole,
  COALESCE(n.nspname, '<global>') AS defaclnamespace,
  d.defaclobjtype,
  d.defaclacl
FROM pg_catalog.pg_default_acl AS d
JOIN pg_catalog.pg_roles AS r ON r.oid = d.defaclrole
LEFT JOIN pg_catalog.pg_namespace AS n ON n.oid = d.defaclnamespace
ORDER BY 1, 2, 3;

-- Test 28: statement (line 109)
GRANT crdb_tests_foo, crdb_tests_bar TO :"cur_user";

-- Test 29: statement (line 112)
ALTER DEFAULT PRIVILEGES FOR ROLE crdb_tests_foo, crdb_tests_bar GRANT ALL ON TABLES TO crdb_tests_foo, crdb_tests_bar WITH GRANT OPTION;

-- Test 30: statement (line 115)
ALTER DEFAULT PRIVILEGES FOR ROLE crdb_tests_foo, crdb_tests_bar GRANT ALL ON TYPES TO crdb_tests_foo, crdb_tests_bar WITH GRANT OPTION;

-- Test 31: statement (line 118)
ALTER DEFAULT PRIVILEGES FOR ROLE crdb_tests_foo, crdb_tests_bar GRANT ALL ON SCHEMAS TO crdb_tests_foo, crdb_tests_bar WITH GRANT OPTION;

-- Test 32: statement (line 121)
ALTER DEFAULT PRIVILEGES FOR ROLE crdb_tests_foo, crdb_tests_bar GRANT ALL ON SEQUENCES TO crdb_tests_foo, crdb_tests_bar WITH GRANT OPTION;

-- Test 33: statement (line 124)
ALTER DEFAULT PRIVILEGES FOR ROLE crdb_tests_foo, crdb_tests_bar GRANT ALL ON FUNCTIONS TO crdb_tests_foo, crdb_tests_bar WITH GRANT OPTION;

-- Test 34: query (line 128)
SELECT
  r.rolname AS defaclrole,
  COALESCE(n.nspname, '<global>') AS defaclnamespace,
  d.defaclobjtype,
  d.defaclacl
FROM pg_catalog.pg_default_acl AS d
JOIN pg_catalog.pg_roles AS r ON r.oid = d.defaclrole
LEFT JOIN pg_catalog.pg_namespace AS n ON n.oid = d.defaclnamespace
ORDER BY 1, 2, 3;

-- Test 35: statement (line 148)
ALTER DEFAULT PRIVILEGES FOR ROLE crdb_tests_foo, crdb_tests_bar REVOKE ALL ON TABLES FROM crdb_tests_foo, crdb_tests_bar;

-- Test 36: statement (line 151)
ALTER DEFAULT PRIVILEGES FOR ROLE crdb_tests_foo, crdb_tests_bar REVOKE ALL ON TYPES FROM crdb_tests_foo, crdb_tests_bar;

-- Test 37: statement (line 154)
ALTER DEFAULT PRIVILEGES FOR ROLE crdb_tests_foo, crdb_tests_bar REVOKE ALL ON SCHEMAS FROM crdb_tests_foo, crdb_tests_bar;

-- Test 38: statement (line 157)
ALTER DEFAULT PRIVILEGES FOR ROLE crdb_tests_foo, crdb_tests_bar REVOKE ALL ON SEQUENCES FROM crdb_tests_foo, crdb_tests_bar;

-- Test 39: statement (line 160)
ALTER DEFAULT PRIVILEGES FOR ROLE crdb_tests_foo, crdb_tests_bar REVOKE ALL ON FUNCTIONS FROM crdb_tests_foo, crdb_tests_bar;

-- Test 40: query (line 165)
SELECT
  r.rolname AS defaclrole,
  COALESCE(n.nspname, '<global>') AS defaclnamespace,
  d.defaclobjtype,
  d.defaclacl
FROM pg_catalog.pg_default_acl AS d
JOIN pg_catalog.pg_roles AS r ON r.oid = d.defaclrole
LEFT JOIN pg_catalog.pg_namespace AS n ON n.oid = d.defaclnamespace
ORDER BY 1, 2, 3;

-- Test 41: statement (line 185)
ALTER DEFAULT PRIVILEGES FOR ROLE crdb_tests_foo GRANT ALL ON TABLES TO crdb_tests_foo;

-- Test 42: statement (line 188)
ALTER DEFAULT PRIVILEGES FOR ROLE crdb_tests_foo GRANT ALL ON SEQUENCES TO crdb_tests_foo;

-- Test 43: statement (line 191)
ALTER DEFAULT PRIVILEGES FOR ROLE crdb_tests_foo GRANT ALL ON SCHEMAS TO crdb_tests_foo;

-- Test 44: statement (line 194)
ALTER DEFAULT PRIVILEGES FOR ROLE crdb_tests_foo GRANT ALL ON TYPES TO crdb_tests_foo;

-- Test 45: statement (line 197)
ALTER DEFAULT PRIVILEGES FOR ROLE crdb_tests_foo GRANT ALL ON FUNCTIONS TO crdb_tests_foo;

-- Test 46: statement (line 200)
ALTER DEFAULT PRIVILEGES FOR ROLE crdb_tests_bar GRANT ALL ON TABLES TO crdb_tests_bar;

-- Test 47: statement (line 203)
ALTER DEFAULT PRIVILEGES FOR ROLE crdb_tests_bar GRANT ALL ON SEQUENCES TO crdb_tests_bar;

-- Test 48: statement (line 206)
ALTER DEFAULT PRIVILEGES FOR ROLE crdb_tests_bar GRANT ALL ON SCHEMAS TO crdb_tests_bar;

-- Test 49: statement (line 209)
ALTER DEFAULT PRIVILEGES FOR ROLE crdb_tests_bar GRANT ALL ON TYPES TO crdb_tests_bar;

-- Test 50: statement (line 212)
ALTER DEFAULT PRIVILEGES FOR ROLE crdb_tests_bar GRANT ALL ON FUNCTIONS TO crdb_tests_bar;

-- Test 51: statement (line 216)
ALTER DEFAULT PRIVILEGES FOR ROLE crdb_tests_foo REVOKE GRANT OPTION FOR ALL ON TABLES FROM crdb_tests_foo;

-- Test 52: statement (line 219)
ALTER DEFAULT PRIVILEGES FOR ROLE crdb_tests_foo REVOKE GRANT OPTION FOR ALL ON SEQUENCES FROM crdb_tests_foo;

-- Test 53: statement (line 222)
ALTER DEFAULT PRIVILEGES FOR ROLE crdb_tests_foo REVOKE GRANT OPTION FOR ALL ON SCHEMAS FROM crdb_tests_foo;

-- Test 54: statement (line 225)
ALTER DEFAULT PRIVILEGES FOR ROLE crdb_tests_foo REVOKE GRANT OPTION FOR ALL ON TYPES FROM crdb_tests_foo;

-- Test 55: statement (line 228)
ALTER DEFAULT PRIVILEGES FOR ROLE crdb_tests_foo REVOKE GRANT OPTION FOR ALL ON FUNCTIONS FROM crdb_tests_foo;

-- Test 56: statement (line 231)
ALTER DEFAULT PRIVILEGES FOR ROLE crdb_tests_bar REVOKE GRANT OPTION FOR ALL ON TABLES FROM crdb_tests_bar;

-- Test 57: statement (line 234)
ALTER DEFAULT PRIVILEGES FOR ROLE crdb_tests_bar REVOKE GRANT OPTION FOR ALL ON SEQUENCES FROM crdb_tests_bar;

-- Test 58: statement (line 237)
ALTER DEFAULT PRIVILEGES FOR ROLE crdb_tests_bar REVOKE GRANT OPTION FOR ALL ON SCHEMAS FROM crdb_tests_bar;

-- Test 59: statement (line 240)
ALTER DEFAULT PRIVILEGES FOR ROLE crdb_tests_bar REVOKE GRANT OPTION FOR ALL ON TYPES FROM crdb_tests_bar;

-- Test 60: statement (line 243)
ALTER DEFAULT PRIVILEGES FOR ROLE crdb_tests_bar REVOKE GRANT OPTION FOR ALL ON FUNCTIONS FROM crdb_tests_bar;

-- Test 61: query (line 248)
SELECT
  r.rolname AS defaclrole,
  COALESCE(n.nspname, '<global>') AS defaclnamespace,
  d.defaclobjtype,
  d.defaclacl
FROM pg_catalog.pg_default_acl AS d
JOIN pg_catalog.pg_roles AS r ON r.oid = d.defaclrole
LEFT JOIN pg_catalog.pg_namespace AS n ON n.oid = d.defaclnamespace
ORDER BY 1, 2, 3;

-- Test 62: statement (line 258)
ALTER DEFAULT PRIVILEGES FOR ROLE crdb_tests_foo GRANT USAGE ON TYPES TO crdb_tests_foo WITH GRANT OPTION;

-- Test 63: query (line 262)
SELECT
  r.rolname AS defaclrole,
  COALESCE(n.nspname, '<global>') AS defaclnamespace,
  d.defaclobjtype,
  d.defaclacl
FROM pg_catalog.pg_default_acl AS d
JOIN pg_catalog.pg_roles AS r ON r.oid = d.defaclrole
LEFT JOIN pg_catalog.pg_namespace AS n ON n.oid = d.defaclnamespace
ORDER BY 1, 2, 3;

-- Test 64: statement (line 273)
ALTER DEFAULT PRIVILEGES FOR ROLE crdb_tests_foo REVOKE GRANT OPTION FOR USAGE ON TYPES FROM crdb_tests_foo;

-- Test 65: query (line 277)
SELECT
  r.rolname AS defaclrole,
  COALESCE(n.nspname, '<global>') AS defaclnamespace,
  d.defaclobjtype,
  d.defaclacl
FROM pg_catalog.pg_default_acl AS d
JOIN pg_catalog.pg_roles AS r ON r.oid = d.defaclrole
LEFT JOIN pg_catalog.pg_namespace AS n ON n.oid = d.defaclnamespace
ORDER BY 1, 2, 3;

-- Cleanup: remove default privileges/privileges that could block role drops.
DROP OWNED BY crdb_tests_foo, crdb_tests_bar, crdb_tests_public;
DROP ROLE crdb_tests_public;
DROP ROLE crdb_tests_bar;
DROP ROLE crdb_tests_foo;

RESET client_min_messages;
