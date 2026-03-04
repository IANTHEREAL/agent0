# Extensions

db9-server extensions are **built-in** (compiled into the server binary) and can be **enabled per-tenant** (TiKV keyspace isolated).

## Capability Matrix

| Capability | Runtime availability | Requires `CREATE EXTENSION` for use | `CREATE EXTENSION` behavior |
|---|---|---|---|
| `vector` type and operators (`<->`, `<#>`, `<=>`) | Built-in, always available | No | Accepted for compatibility; records `pg_extension` metadata |
| `http` (`extensions.http_*`) | Built-in code path | Yes | Installs/enables per-tenant |
| `embedding` (`embedding()`, `extensions.embedding_usage()`) | Built-in code path | Yes | Installs/enables per-tenant |
| `fs9` (`extensions.fs9`) | Built-in code path | Yes | Installs/enables per-tenant |
| `pg_cron` | Built-in code path | Yes | Installs/enables per-tenant |
| `parquet` (`read_parquet`) | Built-in code path (feature-gated at build time) | Yes | Installs/enables per-tenant |
| `uuid-ossp` | Built-in UUID functions | No | Accepted for compatibility; records `pg_extension` metadata |
| `hstore` | Compatibility surface only (catalog/type visibility is install-gated) | Yes | Installs metadata and enables `hstore`/`_hstore` visibility in `pg_type` + `to_regtype` |
| `zhparser` tokenizer | Built-in via jieba | No | Accepted for compatibility; records `pg_extension` metadata |

For ORM/agent bootstrap flows: `CREATE EXTENSION IF NOT EXISTS vector;` is safe but optional. Vector features work without running this statement.

## Regression Contract

The following behavior is a compatibility contract and must not regress:

- `CREATE EXTENSION IF NOT EXISTS vector` must succeed when `vector` is absent.
- Re-running `CREATE EXTENSION IF NOT EXISTS vector` must be idempotent (no error).
- A `pg_extension` row for `vector` must exist after bootstrap.
- Running the bootstrap command inside an explicit transaction must not abort that transaction.

Coverage is enforced by `tests/269_vector_extension_bootstrap.sql`.

## Manage Extensions

```sql
-- Install for the current tenant (requires SUPERUSER)
CREATE EXTENSION http;

-- Uninstall (requires SUPERUSER)
DROP EXTENSION http;

-- List installed extensions
SELECT extname, extversion, extnamespace FROM pg_catalog.pg_extension ORDER BY extname;
```

Notes:
- Extension functions are exposed in the built-in `extensions` schema.
- `CREATE EXTENSION` / `DROP EXTENSION` are only allowed for SUPERUSER.

## Embedding Extension (`embedding`)

After `CREATE EXTENSION embedding`, the following surfaces are available:

- Scalar function: `embedding(text [, model, dimensions]) -> vector`
- Table function: `extensions.embedding_usage() -> (tokens_used BIGINT, resets_at TIMESTAMPTZ)`

Behavior contract:

- SUPERUSER-only execution (`embedding: permission denied (superuser required)`).
- Extension gate is enforced before execution (including `embedding(NULL)`).
- Visibility is controlled by extension install state plus in-transaction DDL delta:
  `CREATE/DROP EXTENSION` in the current transaction takes precedence.
- With no in-transaction override:
  - explicit-transaction statements use a transaction-consistent visibility source,
  - autocommit statements use latest committed extension metadata at statement boundary.
- Missing extension visibility must surface as function-not-found semantics (`42883`), not `0A000`.
- Compatibility note: db9 follows a **PG-compatible by default** strategy, but
  visibility behavior under concurrent extension DDL is being hardened toward a
  single deterministic model across catalog and function-gate surfaces.
  See design record: `docs/design/28_embedding_extension_pg_parity_contract.md`
  and follow-up issue `#1421`.
- Function signature mismatches (wrong arity/type) must fail at function-resolution time with `42883`.
- PostgreSQL-style literal coercion applies to `dimensions`: quoted numeric literals like `'1024'` are accepted; non-literal text expressions require explicit cast.
- Runtime `22023` is reserved for value-domain validation on valid signatures (for example, invalid dimensions value).
- Model is pinned to `text-embedding-v4` (case-insensitive). Other model names are rejected.

Configuration knobs:

- Env: `EMBEDDING_API_KEY`, `EMBEDDING_ENDPOINT` (or `EMBEDDING_BASE_URL`), `EMBEDDING_MODEL`, `EMBEDDING_DIMENSIONS`
- Session GUCs: `embedding.model`, `embedding.dimensions`, `embedding.max_calls`, `embedding.concurrency`

Notes:

- Service is unavailable when `EMBEDDING_API_KEY` is unset.
- Endpoint values are normalized to an `/embeddings` path.
- Normative contract source: `docs/sot/extensions-gin.md`.
- Design rationale and decision history: `docs/design/28_embedding_extension_pg_parity_contract.md`.

## HTTP Extension (`http`)

After `CREATE EXTENSION http`, the following table functions are available:

- `extensions.http_get(url TEXT)`
- `extensions.http_head(url TEXT)`
- `extensions.http_delete(url TEXT)`
- `extensions.http_post(url TEXT, body TEXT, content_type TEXT)`
- `extensions.http_put(url TEXT, body TEXT, content_type TEXT)`

All of them return a single-row virtual table with columns:

| Column | Type | Notes |
|--------|------|-------|
| `status` | `INT` | HTTP status code |
| `content_type` | `TEXT` | Nullable; from `Content-Type` |
| `headers` | `JSONB` | JSON array of `{ "field": "...", "value": "..." }` |
| `content` | `TEXT` | Response body (must be valid UTF-8) |

Example:

```sql
SELECT status, content_type, headers, content
FROM extensions.http_get('https://example.com');
```

### Security & Limits

- SUPERUSER-only execution (non-superusers get `permission denied for extension "http"`).
- By default, only `https://` URLs on port `443` are allowed.
- **Insecure HTTP support**: Set `DB9_HTTP_ALLOW_INSECURE=true` to enable `http://` URLs on port `80`. Use with caution as HTTP traffic is unencrypted.
- SSRF protection blocks `localhost`, `.localhost`, `.local`, and any URL that resolves to loopback/private/link-local/unspecified IP ranges.
- Limits (currently fixed in code): connect timeout 1s, total timeout 5s, max request body 256KiB, max response 1MiB, max redirects 3 (GET/POST/PUT/DELETE only), max 5 HTTP calls per SQL statement, max 20 in-flight requests per tenant per node.

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `DB9_HTTP_ALLOW_INSECURE` | `false` | Set to `true` or `1` to allow insecure HTTP requests (port 80) |
