# fs9 Authentication — Direct auth9 Mint (replaces db9-backend exchange)

**Status**: Active

Lands with PR #2547 and supersedes the exchange-based design previously
described in `fs9_v2_restore_progress.md`.

## Problem

The fs9 v2 restore on `fsplane.v2` (#2545) wired the SQL/WS fs9 paths so
db9-server forwards an `aud="fs-plane"` JWT to every fs9 RPC. To obtain
that token, db9-server POSTs to db9-backend's
`/internal/connect-token/exchange`, which turns around and calls auth9
`POST /v1/jwt/sign`. db9-backend in this flow is **a pass-through
translator**: it does not contribute an independent authority — it has
the same `X-API-Key` to auth9 that db9-server can hold, and it mints
with the same claim shape db9-server can construct locally.

Two concrete consequences of routing through db9-backend:

1. **The exchange endpoint requires the customer's `aud="db9-server"`
   connect-token as a `Bearer`.** That makes it structurally impossible
   for non-JWT sessions (password / `db9ck_*` connect-key) to reach
   fs9 — `init_juicefs_backend` hard-rejects them at `backend.rs:904`.
   But db9-server already authenticates those sessions via its own
   path (`db9_auth.rs::dispatch_db9_auth`). The "can't mint fs-plane"
   gap is not a security boundary; it's an artefact of treating the
   customer's connect-token as the exchange authority.

2. **Every fs9 call costs one extra synchronous HTTP hop** (db9-server
   → db9-backend → auth9) on the token-cache miss path. The hop adds
   no signing authority — auth9 is the signer in both shapes.

## Decision

Cut db9-backend out of the fs9 path. db9-server calls auth9
`POST /v1/jwt/sign` directly, with the same `X-API-Key` it already
holds (`DB9_AUTH9_SERVICE_API_KEY` — used today for
`POST /v1/credentials/verify` on the connect-key path).

This does not turn db9-server into an issuer. auth9 is still the only
holder of the signing key, and the audience whitelist
(`services.<id>.allowed_audiences`) gates which audiences each caller
may request — the same gate db9-backend passes through today.

## Trust model

db9-server's role: **authenticator + token-requester**.

- *Authenticator*: db9-server alone decides whether a pgwire session
  is valid. It accepts three login methods (JWT, `db9ck_*` connect-key,
  password). All three produce the same downstream session shape.

- *Token-requester*: at fs9-backend init time, db9-server asks auth9 to
  sign an `aud="fs-plane"` JWT whose claims match the authenticated
  session. db9-server's authority to ask is its `X-API-Key`; auth9's
  authority to sign is its private key.

The capability "request fs-plane mint" was already present in this
deployment, held by db9-backend. The change moves it to db9-server.
Compromise blast radius is unchanged: a compromised db9-server today
could obtain fs-plane tokens for any tenant via the exchange endpoint
(db9-backend trusts db9-server's `X-API-Key`); after this change, the
same compromise hits auth9 directly with the same authority. No new
power is conferred.

## Three-login coherence

Why the change must address all three login methods, not just JWT:

| Login method | What db9-server has after auth | Material for fs9 today | After this change |
|---|---|---|---|
| JWT (3-segment) | Raw token + verified claims | Forwarded as Bearer to db9-backend exchange | db9-server mints via auth9 |
| Connect-key (`db9ck_*`) | tenant_id, role | None — hard-rejected | db9-server mints via auth9 |
| Password | tenant_id, role (local bcrypt verify) | None — hard-rejected | db9-server mints via auth9 |

Treating the three paths uniformly is a coherence requirement, not a
discretion: db9-server promises to accept all three logins. Letting fs9
work only for JWT logins would be authentication-method leakage into
the capability model.

`(tenant_id, role)` is the only material the mint path needs. fs9 v2's
interceptor reads `aud`, `tid`, `scp`, `jti` from the JWT and ignores
`sub`. db9-server therefore mints without a `sub` claim — a per-session
value would only create the illusion of an audit contract that fs9
doesn't honour, and would leak across sessions through the
`(tenant_id, role)` cache.

## Mint contract

```
POST {AUTH9_SIGN_URL}/v1/jwt/sign
Headers:
  X-API-Key: $DB9_AUTH9_SERVICE_API_KEY
  Content-Type: application/json
Body:
{
  "aud": "fs-plane",
  "ttl_secs": 900,
  "claims": {
    "tid": "<tenant_id>",
    "usr": "<tenant_id>.<role>",
    "scp": "fs:volume:jfs_t_<tenant_id>:rw"   // or :r for readonly
  }
}

200 OK
{
  "token": "<jwt>",
  "expires_at": "<RFC3339>"
}
```

`aud` is a single string (auth9's `SignBody.aud: String`); a
multi-audience variant is out of scope here.

## Role → scope map

- `admin` → `fs:volume:jfs_t_<tid>:rw`
- `_db9_sys_readonly` → `fs:volume:jfs_t_<tid>:r`
- anything else → reject the backend init (do not silently downgrade)

## TTL and cache

- `ttl_secs` requested: 900s. auth9 may clamp lower via
  `services.<id>.max_jwt_ttl_secs`; the cache honours the `expires_at`
  it returns.
- Cache key: `(tenant_id, role)`. The minted body contains no
  per-session fields, so cache hits are safe to share across sessions.
- Refresh lead: 60s before `expires_at`. Stale entries are evicted on
  the miss-lookup path.

## Config

| Env var | Used for | Required for JuiceFS tenants |
|---|---|---|
| `AUTH9_SIGN_URL` | full URL of auth9 `/v1/jwt/sign` (no trailing slash) | yes |
| `DB9_AUTH9_SERVICE_API_KEY` | `X-API-Key` for both verify and sign | yes |
| `FS9_GRPC_ENDPOINT` | fs9 v2 gRPC endpoint | yes |
| `FS9_GRPC_TLS_SERVER_NAME` | TLS SNI for fs9 gRPC | yes |

Removed: `DB9_BACKEND_URL`, `DB9_SERVER_API_KEY` (exchange-path only).

## auth9 pre-requisite

`db9-server`'s service entry in auth9 config (`services.db9-server`)
must include `"fs-plane"` in `allowed_audiences`. This was previously
on db9-backend's entry. Until that config lands, sign requests return
`403 audience_not_allowed` and JuiceFS tenants surface a hard error at
backend init — no silent fallback.

## Invariants preserved

- **CLAUDE.md "verify, don't mint"**: db9-server still does not hold
  any JWT signing key. It is a requester to the signer (auth9), the
  same role db9-backend played.
- **Multi-tenancy isolation**: every claim is derived from the
  authenticated session's `(tenant_id, role)`; no cross-tenant claim
  construction is possible without a corresponding authenticated
  session.
- **Audit chain**: auth9 emits `jwt.signed` with `service_id` and
  `aud`. Replaces today's db9-backend
  `connect-token.exchange.signed` audit event with an equivalent at
  auth9. Per-session attribution belongs in db9-server's own access
  log, not in the fs-plane JWT.

## Migration

Single-PR migration is safe because the exchange path is fully
replaced, not gated:

1. Land auth9 config update adding `fs-plane` to db9-server's
   `allowed_audiences`. (Out-of-band; no code change.)
2. Land this PR. Both code and deployment env vars switch in one go.
3. Remove db9-backend's `/internal/connect-token/exchange` endpoint in
   a follow-up once no caller remains. (Tracked separately; not
   blocking this PR.)

Roll-forward only; the exchange path is deleted in this PR, not
flagged. If staging surfaces a regression, revert the PR.
