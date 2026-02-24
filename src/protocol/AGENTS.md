# Protocol Module

PostgreSQL wire protocol via pgwire. ~12,000 lines across 28 files.

## Layout

```
src/protocol/
├── mod.rs                      # Module exports
├── copy_format.rs              # COPY format parsing (CSV, TEXT, BINARY) (~900 lines)
└── handler/
    ├── mod.rs                  # Handler exports, connection ID counter, utility functions
    ├── dynamic/                # DynamicPgHandler — main pgwire handler (~2,200 lines)
    │   ├── mod.rs              # DynamicPgHandler struct, DynamicHandlerFactory
    │   ├── query.rs            # Extended/simple query protocol handling
    │   ├── copy.rs             # COPY FROM STDIN / COPY TO STDOUT protocol
    │   └── startup.rs          # Authentication + executor initialization
    ├── encode/                 # Value encoding + type mapping (~840 lines)
    │   ├── mod.rs              # Re-exports
    │   ├── types.rs            # DataType <-> PostgreSQL OID mapping
    │   ├── result.rs           # result_to_response(): ExecuteResult → pgwire Response
    │   └── value.rs            # Internal types → pgwire wire format
    ├── params/                 # Parameter counting + decoding (~480 lines)
    │   ├── mod.rs              # Re-exports
    │   ├── scan.rs             # count_sql_parameters(): $N placeholder counting
    │   └── decode.rs           # decode_parameters(): wire bytes → Value (Bind phase)
    ├── copy/                   # COPY context management (~100 lines)
    │   └── mod.rs              # CopyContext struct, push_copy_data(), max line size (32MB)
    ├── portal.rs               # Portal state management + suspended portal handling (~430 lines)
    ├── query_parser.rs         # Db9QueryParser: pgwire QueryParser trait (~110 lines)
    ├── server_params.rs        # PgServerParameterProvider: ParameterStatus (~150 lines)
    ├── tenant.rs               # parse_tenant_username(): multi-tenancy (~50 lines)
    ├── errors.rs               # SQLSTATE mapping, error helpers (~75 lines)
    ├── prepared.rs             # Prepared statement facade (re-exports from SQL layer)
    └── tests.rs                # Comprehensive protocol test suite (~2,700 lines)
```

## Handlers

- `DynamicPgHandler` — Production handler (per-connection TiKV client)
- `DynamicHandlerFactory` — Creates handlers with keyspace from username

## Protocol Flows

**Simple Query:**
```
Query("SELECT ...") → on_query() → executor.execute() → result_to_response()
```

**Extended Query (ORMs):**
```
Parse → Analyze → Bind → Describe → Execute
                           ↓
             AnalyzedQuery output_schema → PostgreSQL OIDs
```

For data statements (SELECT/INSERT/UPDATE/DELETE), `on_parse` runs the Analyzer
to produce typed IR with `output_schema`. Describe reads this schema directly.

For utility statements (DDL/SET/SHOW), `on_parse` keeps `RawSqlUtility` and
Describe uses `utility_describe_fields()` for static schema mapping.

## Where to Look

| Task | Location |
|------|----------|
| Add PostgreSQL type | `encode/types.rs` → `datatype_to_pgtype()` |
| Fix type OID | Analyzer `output_schema` (analyzer/query/) |
| Change value encoding | `encode/result.rs` → `encode_value()` / `encode/value.rs` |
| Fix parameter decoding | `params/decode.rs` → `decode_parameters()` |
| Portal / suspended queries | `portal.rs` |
| Multi-tenancy parsing | `tenant.rs` → `parse_tenant_username()` |
| Server parameters | `server_params.rs` → `PgServerParameterProvider` |
| COPY protocol | `dynamic/copy.rs` + `copy/mod.rs` + `copy_format.rs` |
| Error handling | `errors.rs` (SQLSTATE mapping, in-failed-transaction errors) |
| Query parsing (pgwire trait) | `query_parser.rs` → `Db9QueryParser` |

## Key Functions

| Function | Location | Purpose |
|----------|----------|---------|
| `parse_tenant_username()` | `tenant.rs` | Multi-tenancy: extract keyspace from "tenant.user" |
| `is_data_statement()` | `dynamic/query.rs` | Classify SELECT/DML statements |
| `reject_unanalyzed_if_needed()` | `dynamic/query.rs` | Guard unanalyzed data SQL |
| `utility_describe_fields()` | `dynamic/query.rs` | Static Describe for utility statements |
| `count_sql_parameters()` | `params/scan.rs` | Count $N placeholders |
| `decode_parameters()` | `params/decode.rs` | Wire bytes → Value |
| `datatype_to_pgtype()` | `encode/types.rs` | Internal type → PostgreSQL OID |
| `result_to_response()` | `encode/result.rs` | ExecuteResult → pgwire Response |
| `Db9QueryParser::parse_sql()` | `query_parser.rs` | Parse + analyze SQL (pgwire QueryParser trait) |

## PostgreSQL Type OIDs

| Internal | PostgreSQL | OID |
|----------|------------|-----|
| Int32 | INT4 | 23 |
| Int64 | INT8 | 20 |
| Float64 | FLOAT8 | 701 |
| Boolean | BOOL | 16 |
| Text | TEXT | 25 |
| Timestamp | TIMESTAMPTZ | 1184 |
| Uuid | UUID | 2950 |
| Jsonb | JSONB | 3802 |

## Multi-Tenancy

Username format: `tenant.user` or `tenant:user`
- `tenant.rs` extracts keyspace from username
- Each keyspace gets isolated TiKV client
- Pooled in `src/pool.rs`

Metadata keys passed via pgwire ClientInfo:
- `METADATA_KEYSPACE` — Extracted tenant keyspace
- `METADATA_ACTUAL_USER` — Username after tenant.user parsing
- `METADATA_AUTH_IS_SUPERUSER` — Authentication status ("on"/"off")

## Common Issues

**Wrong OIDs (integers as strings):**
- Check Analyzer `output_schema` for correct DataType
- Check `column_types` in executor result

**Describe returns empty for SHOW/EXPLAIN:**
- Check `utility_describe_fields()` in `dynamic/query.rs`

**Portal suspension / cursor issues:**
- Check `portal.rs` for portal state management
