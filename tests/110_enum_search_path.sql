-- search_path resolution for enum CREATE TYPE / DROP TYPE

-- Cleanup from prior runs
DROP TYPE IF EXISTS app.role;
DROP TYPE IF EXISTS public.role;
DROP SCHEMA IF EXISTS app;

CREATE SCHEMA app;

SET search_path TO app, public;

CREATE TYPE role AS ENUM ('USER', 'ADMIN');

-- Unqualified CREATE TYPE should create in search_path[0]
SELECT 1 / (CASE WHEN
  EXISTS (
    SELECT 1
    FROM pg_catalog.pg_type t
    JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace
    WHERE n.nspname = 'app' AND t.typname = 'role'
  )
  AND NOT EXISTS (
    SELECT 1
    FROM pg_catalog.pg_type t
    JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace
    WHERE n.nspname = 'public' AND t.typname = 'role'
  )
THEN 1 ELSE 0 END);

SET search_path TO public, app;

-- Unqualified DROP TYPE should resolve via search_path
DROP TYPE role;

SELECT 1 / (CASE WHEN NOT EXISTS (
  SELECT 1
  FROM pg_catalog.pg_type t
  JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace
  WHERE n.nspname IN ('app', 'public') AND t.typname = 'role'
) THEN 1 ELSE 0 END);

-- Cleanup
DROP SCHEMA app;
SET search_path TO DEFAULT;

