SET client_min_messages = warning;
DROP SCHEMA IF EXISTS pr1661_type_shadow_ddl CASCADE;
RESET client_min_messages;
CREATE SCHEMA pr1661_type_shadow_ddl;
CREATE TYPE pr1661_type_shadow_ddl.text AS ENUM ('x');

SET search_path = pr1661_type_shadow_ddl, public;

DROP TYPE text;
ALTER TYPE text RENAME TO text2;

SELECT
    'shadow='
        || count(*) FILTER (WHERE t.typname = 'text')
        || '|renamed='
        || count(*) FILTER (WHERE t.typname = 'text2') AS shadow_state
FROM pg_type t
JOIN pg_namespace n ON n.oid = t.typnamespace
WHERE n.nspname = 'pr1661_type_shadow_ddl';

RESET search_path;
DROP TYPE pr1661_type_shadow_ddl.text;
DROP SCHEMA pr1661_type_shadow_ddl;
