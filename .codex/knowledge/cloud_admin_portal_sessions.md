# cloud-admin-portal Sessions — per-tenant sessionStorage key

## What
- Tenant admin sessions are stored in browser `sessionStorage` per tenant:
  - Key: `tenant_session:<tenant>`
  - Value: `session_id` returned by `POST /api/tenants/{name}/connect`

## Why
- The backend stores sessions in-memory (expires ~1h; lost on backend restart).
- Storing the session id globally causes cross-tenant mixups when switching tenants (Tenant A session sent to Tenant B endpoints → `401 Invalid or expired session`).

## Where
- Frontend attaches the session header based on request path:
  - `cloud-admin-portal/frontend/src/api/client.ts` → adds `X-Tenant-Session` for `/tenants/{name}/...`
- Frontend session state management:
  - `cloud-admin-portal/frontend/src/hooks/useTenantSession.tsx`

## Notes
- Observability endpoint does not require tenant session; it uses the per-tenant `_db9_sys_observer` credentials stored in portal DB.

