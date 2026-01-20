# Extensions

pg-tikv extensions are **built-in** (compiled into the server binary) and can be **enabled per-tenant** (TiKV keyspace isolated).

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
- Only `https://` URLs are allowed, and only port `443`.
- SSRF protection blocks `localhost`, `.localhost`, `.local`, and any URL that resolves to loopback/private/link-local/unspecified IP ranges.
- Limits (currently fixed in code): connect timeout 1s, total timeout 5s, max request body 256KiB, max response 1MiB, max redirects 3 (GET/POST/PUT/DELETE only), max 5 HTTP calls per SQL statement, max 20 in-flight requests per tenant per node.
