# Multi-Tenancy Architecture

> **Contracts**: See [docs/sot/multi-tenancy.md](../sot/multi-tenancy.md) for normative specifications.
> **User guide**: See [docs/multi-tenancy.md](../multi-tenancy.md) for setup instructions and examples.

## Tenant Routing

```
Username format: "tenant.user" or "tenant:user"
    → parse_tenant_username() extracts keyspace
    → pool.acquire(keyspace) returns tenant-isolated TiKV client
    → All KV operations scoped to keyspace prefix
    → Stats cache, schema cache isolated per tenant
```

## Keyspace Isolation Model

Each tenant's data lives in a separate TiKV keyspace. The isolation is enforced at the connection pool level — a connection bound to one keyspace cannot access data from another.

Key isolation points:
- `parse_tenant_username()` (`src/protocol/handler/tenant.rs`): extracts keyspace from username at connection time
- `TikvClientPool` (`src/pool.rs`): maintains per-keyspace TiKV clients
- `TenantEntry`: per-tenant `TableStatsCache` and schema cache
- Keyspace is immutable for the lifetime of a connection

## Default Keyspace

When a username contains no separator (`.` or `:`), the connection routes to the keyspace specified by `PG_KEYSPACE` (default: `"default"`).
