-- Issue #417 regression: pg_catalog type reflection must cover numeric/time/interval.

DROP TABLE IF EXISTS public.ty_417;

CREATE TABLE public.ty_417(n numeric, t time, i interval);

-- pg_type must expose these built-in type OIDs (time was the biggest gap).
SELECT oid, typname FROM pg_type WHERE oid IN (1700,1083,1186) ORDER BY 1;

-- pg_attribute.atttypid + format_type must not fall back to text (OID=25).
SELECT a.attname,
       a.atttypid,
       format_type(a.atttypid, a.atttypmod) AS formatted
  FROM pg_attribute a
  JOIN pg_class c ON a.attrelid = c.oid
  JOIN pg_namespace n ON c.relnamespace = n.oid
 WHERE n.nspname = 'public'
   AND c.relname = 'ty_417'
 ORDER BY a.attnum;

DROP TABLE public.ty_417;
