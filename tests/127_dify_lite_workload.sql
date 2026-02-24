-- Dify-lite workload (post-restore) for db9-server
--
-- Requires: `tests/96_dify_schema.sql` has already restored the upstream Dify schema into database `dify_compat_96`.
-- Goal: small, deterministic, hermetic workload traceable to real Dify behavior.
--
-- Covered patterns:
-- - tenant/account membership joins (accounts/tenants/tenant_account_joins)
-- - timezone day bucketing (DATE(DATE_TRUNC('day', ts AT TIME ZONE 'UTC' AT TIME ZONE :tz)))
-- - keyword search across JSON array elements (jsonb_array_elements_text + ILIKE ... ESCAPE)
-- - plugin daemon init migration pattern (ALTER COLUMN ... DROP NOT NULL + information_schema introspection)

\connect dify_compat_96

BEGIN;

-- Deterministic IDs (avoid comparing random UUID outputs).
INSERT INTO public.tenants (id, name, plan, status, created_at, updated_at, custom_config)
VALUES (
    '00000000-0000-0000-0000-000000000001'::uuid,
    'Acme Workspace',
    'basic',
    'normal',
    '2026-01-01 00:00:00'::timestamp,
    '2026-01-01 00:00:00'::timestamp,
    NULL
);

INSERT INTO public.accounts (id, name, email, status, timezone, created_at, updated_at, last_active_at)
VALUES (
    '00000000-0000-0000-0000-000000000002'::uuid,
    'Alice',
    'alice@example.com',
    'active',
    'Asia/Shanghai',
    '2026-01-01 00:00:00'::timestamp,
    '2026-01-01 00:00:00'::timestamp,
    '2026-01-01 00:00:00'::timestamp
);

INSERT INTO public.tenant_account_joins (id, tenant_id, account_id, role, current, created_at, updated_at)
VALUES (
    '00000000-0000-0000-0000-000000000003'::uuid,
    '00000000-0000-0000-0000-000000000001'::uuid,
    '00000000-0000-0000-0000-000000000002'::uuid,
    'owner',
    true,
    '2026-01-01 00:00:00'::timestamp,
    '2026-01-01 00:00:00'::timestamp
);

INSERT INTO public.datasets (id, tenant_id, name, created_by, created_at, updated_at)
VALUES (
    '00000000-0000-0000-0000-000000000010'::uuid,
    '00000000-0000-0000-0000-000000000001'::uuid,
    'acme-dataset',
    '00000000-0000-0000-0000-000000000002'::uuid,
    '2026-01-01 00:00:00'::timestamp,
    '2026-01-01 00:00:00'::timestamp
);

-- Use a timestamp near midnight UTC to make the timezone/day bucketing unambiguous.
INSERT INTO public.documents (
    id,
    tenant_id,
    dataset_id,
    "position",
    data_source_type,
    batch,
    name,
    created_from,
    created_by,
    created_at,
    updated_at
)
VALUES (
    '00000000-0000-0000-0000-000000000020'::uuid,
    '00000000-0000-0000-0000-000000000001'::uuid,
    '00000000-0000-0000-0000-000000000010'::uuid,
    1,
    'upload_file',
    'batch-1',
    'hello.txt',
    'upload_file',
    '00000000-0000-0000-0000-000000000002'::uuid,
    '2026-01-02 23:00:00'::timestamp,
    '2026-01-02 23:00:00'::timestamp
);

INSERT INTO public.document_segments (
    id,
    tenant_id,
    dataset_id,
    document_id,
    "position",
    content,
    word_count,
    tokens,
    keywords,
    hit_count,
    created_by,
    created_at,
    updated_at
)
VALUES (
    '00000000-0000-0000-0000-000000000030'::uuid,
    '00000000-0000-0000-0000-000000000001'::uuid,
    '00000000-0000-0000-0000-000000000010'::uuid,
    '00000000-0000-0000-0000-000000000020'::uuid,
    1,
    'Hello 100% world',
    3,
    3,
    '["你好","world","100%"]'::json,
    0,
    '00000000-0000-0000-0000-000000000002'::uuid,
    '2026-01-02 23:00:01'::timestamp,
    '2026-01-02 23:00:01'::timestamp
);

COMMIT;

-- Dify backend: tenant/account membership join query shape.
SELECT 'dify_lite.member_join_count=' || (
    SELECT COUNT(*)
    FROM public.tenant_account_joins ta
    JOIN public.tenants t ON t.id = ta.tenant_id
    JOIN public.accounts a ON a.id = ta.account_id
    WHERE ta.tenant_id = '00000000-0000-0000-0000-000000000001'::uuid
      AND ta.account_id = '00000000-0000-0000-0000-000000000002'::uuid
);

SELECT 'dify_lite.member_role=' || (
    SELECT role
    FROM public.tenant_account_joins
    WHERE tenant_id = '00000000-0000-0000-0000-000000000001'::uuid
      AND account_id = '00000000-0000-0000-0000-000000000002'::uuid
    LIMIT 1
);

-- Dify backend: convert_datetime_to_date() expression (DATE(DATE_TRUNC('day', ... AT TIME ZONE ...))).
SELECT 'dify_lite.doc_day_shanghai=' || (
    SELECT DATE(DATE_TRUNC('day', created_at AT TIME ZONE 'UTC' AT TIME ZONE 'Asia/Shanghai'))
    FROM public.documents
    WHERE id = '00000000-0000-0000-0000-000000000020'::uuid
);

-- Dify backend: keyword search over JSON array elements + ESCAPE handling.
SELECT 'dify_lite.segment_keywords_match_100pct=' || (
    SELECT CASE WHEN EXISTS (
        SELECT 1
        FROM public.document_segments ds
        WHERE ds.id = '00000000-0000-0000-0000-000000000030'::uuid
          AND array_to_string(
              ARRAY(SELECT jsonb_array_elements_text(CAST(ds.keywords AS jsonb))),
              ','
              ) ILIKE '%100\%%' ESCAPE '\'
    ) THEN 't' ELSE 'f' END
);

-- Dify plugin daemon: init-time schema migration pattern (`ALTER COLUMN ... DROP NOT NULL`).
DROP TABLE IF EXISTS public.plugins;
CREATE TABLE public.plugins (
    id uuid PRIMARY KEY,
    declaration text NOT NULL
);

SELECT 'dify_lite.plugin_has_declaration_column=' || (
    SELECT CASE WHEN EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_schema = 'public'
          AND table_name = 'plugins'
          AND column_name = 'declaration'
    ) THEN 't' ELSE 'f' END
);

ALTER TABLE public.plugins ALTER COLUMN declaration DROP NOT NULL;

SELECT 'dify_lite.plugin_declaration_nullable=' || (
    SELECT is_nullable
    FROM information_schema.columns
    WHERE table_schema = 'public'
      AND table_name = 'plugins'
      AND column_name = 'declaration'
);

-- Explicit cleanup: drop the dedicated restore database to keep the shared test environment clean.
\connect postgres
DROP DATABASE dify_compat_96;
