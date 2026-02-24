# Protocol Layer Architecture

> **Contracts**: See [docs/sot/protocol-pgwire.md](../sot/protocol-pgwire.md) for normative specifications.
> **Navigation**: See [src/protocol/AGENTS.md](../../src/protocol/AGENTS.md) for detailed code paths and symbols.
> **Extended protocol**: See [docs/prepared-statement-contract.md](../prepared-statement-contract.md) for prepared statement semantics.

## Connection Lifecycle

```
1. Client connects via TCP
2. pgwire Startup message received
3. parse_tenant_username("tenant.user") → (keyspace, username)
4. Authentication challenge (CleartextPassword)
5. Verify credentials against TiKV-stored auth data
6. init_executor(): acquire TiKV client from pool (keyspace-isolated)
7. Session ready for queries
```

## Query Handling

| Protocol | Handler | Description |
|----------|---------|-------------|
| Simple Query | `on_query()` | Direct SQL text, supports `;`-separated statements |
| Extended Query | `on_bind()` + `on_execute()` | Prepared statements with parameter binding |
| COPY | `on_copy_data()` + `on_copy_done()` | Bulk data loading |
| Describe | `on_describe_statement()` | Return parameter types + result columns |

## Handler Decomposition

The protocol handler (`src/protocol/handler/`) is decomposed into focused modules:

| Module | Purpose |
|--------|---------|
| `dynamic/` | `DynamicPgHandler`: main pgwire handler (mod.rs, query.rs, copy.rs, startup.rs) |
| `portal.rs` | Portal state management and suspended portal handling |
| `query_parser.rs` | `Db9QueryParser`: SQL parsing + analysis (pgwire QueryParser trait) |
| `server_params.rs` | `PgServerParameterProvider`: ParameterStatus for pgwire |
| `tenant.rs` | `parse_tenant_username()`: multi-tenancy username parsing |
| `errors.rs` | Error helpers: SQLSTATE mapping, in-failed-transaction errors |
| `encode/` | Value encoding + type mapping (types.rs, result.rs, value.rs) |
| `params/` | Parameter counting + decoding (scan.rs, decode.rs) |
| `copy/` | COPY context management |
