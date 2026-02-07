-- Issue #416 regression: list_tables cache must be invalidated after ALTER TABLE ... RENAME

DROP TABLE IF EXISTS public.t_cache;
DROP TABLE IF EXISTS public.t_cache2;

CREATE TABLE public.t_cache(a int);

-- Warm list_tables cache via information_schema
SELECT 'warmup' AS phase, table_name
  FROM information_schema.tables
 WHERE table_schema = 'public'
   AND table_name = 't_cache';

ALTER TABLE public.t_cache RENAME TO t_cache2;

-- Must reflect rename immediately (no TTL wait), and must not produce phantom rows.
SELECT 'after' AS phase, table_name
  FROM information_schema.tables
 WHERE table_schema = 'public'
   AND table_name IN ('t_cache', 't_cache2')
 ORDER BY table_name;

DROP TABLE public.t_cache2;
