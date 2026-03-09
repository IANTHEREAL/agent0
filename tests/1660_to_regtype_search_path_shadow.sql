SET client_min_messages = warning;
DROP SCHEMA IF EXISTS pr1660_to_regtype_shadow CASCADE;
RESET client_min_messages;
CREATE SCHEMA pr1660_to_regtype_shadow;
CREATE TYPE pr1660_to_regtype_shadow.text AS ENUM ('x');

SET search_path = pr1660_to_regtype_shadow, pg_catalog;

SELECT
    to_regtype('pr1660_to_regtype_shadow.text') = to_regtype('text') AS shadow_wins,
    to_regtype('text') = to_regtype('pg_catalog.text') AS builtin_loses;

RESET search_path;
DROP TYPE pr1660_to_regtype_shadow.text;
DROP SCHEMA pr1660_to_regtype_shadow;
