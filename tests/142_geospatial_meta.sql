-- PostgreSQL compatible tests from geospatial_meta
-- 13 tests

SET client_min_messages = warning;

-- Setup: PostGIS is required for the PostGIS_* metadata functions.
CREATE EXTENSION IF NOT EXISTS postgis;

-- Test 1: query (line 3)
SELECT PostGIS_Extensions_Upgrade();

-- Test 2: query (line 8)
SELECT PostGIS_Full_Version();

-- Test 3: query (line 13)
SELECT PostGIS_GEOS_Version();

-- Test 4: query (line 18)
SELECT PostGIS_LibXML_Version();

-- Test 5: query (line 23)
SELECT PostGIS_Lib_Build_Date();

-- Test 6: query (line 28)
SELECT PostGIS_Lib_Version();

-- Test 7: query (line 33)
SELECT PostGIS_Liblwgeom_Version();

-- Test 8: query (line 38)
SELECT PostGIS_PROJ_Version();

-- Test 9: query (line 43)
SELECT PostGIS_Scripts_Build_Date();

-- Test 10: query (line 48)
SELECT PostGIS_Scripts_Installed();

-- Test 11: query (line 53)
SELECT PostGIS_Scripts_Released();

-- Test 12: query (line 58)
SELECT PostGIS_Version();

-- Test 13: query (line 63)
SELECT PostGIS_Wagyu_Version();

RESET client_min_messages;
