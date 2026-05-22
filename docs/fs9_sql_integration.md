# fs9 ↔ SQL Integration Reference

> **Non-SoT note.** This document describes how the SQL layer in
> db9-server integrates with the fs9 filesystem subsystem, derived
> from reading the implementation. Authoritative contracts live in
> `docs/sot/**` and the code under `src/extensions/fs/`,
> `src/sql/expr/functions/fs9.rs`, `src/sql/executor/extensions.rs`,
> and `src/sql/executor/table_functions.rs`. Validate current
> behavior against those before treating this document as a
> contract.

Audience: db9-server developers, fs9 maintainers, SQL execution-path
contributors.

Related:
- `docs/fs9_extension.md` — user-facing manual. **Out of date in
  several places** (size cap, "superuser only" universal claim);
  see §8 and §11.
- `docs/design/29_fs9_v2_tikv_metadata_s3_packfiles.md` — v2 storage
  architecture.
- `docs/design/33_fs_plane_juicefs_data_engine.md` — fs-plane gRPC
  proxy and JuiceFS backend (sealed-file vs FsPlane asymmetry, §6).
- `docs/design/26_fs9_websocket_api.md` — WS surface (same backend,
  different transport).
- `docs/design/fs9_auth9_direct_mint.md` — auth9 direct token mint
  (current head).

## §0. What this document is and isn't

| Is | Isn't |
|---|---|
| A current-behavior reference at commit `9d8488d2` | A normative contract — `docs/sot/**` is |
| Honest about correctness bugs in master (KNOWN ISSUE callouts below) | A roadmap or wishlist |
| Source of the cross-surface invariant (SQL ⇔ WS ⇔ FUSE share one namespace) | A re-derivation of design 29 / 33 — those are linked, not duplicated |

Every behavioral claim cites `path/file.rs:LINE` against master
`9d8488d2`. Where master ships a known bug, the section is tagged
**KNOWN ISSUE** with a one-line explanation and the file:line that
needs to change.

---

## 1. Goal of the Integration

fs9 exposes a per-tenant filesystem namespace (directories, regular
files, symlinks) backed by one of two storage engines per tenant:

- **embedded**: TiKV-backed PageFs in the tenant's `db9_tenant_<id>`
  keyspace (`src/extensions/fs/embedded/`).
- **JuiceFS**: JuiceFS metadata in the tenant's `jfs_t_<id>`
  keyspace, data in S3, served via the fs-plane gRPC proxy
  (`src/extensions/fs/grpc/`).

The SQL integration makes that namespace addressable from SQL so
that:

- file contents can be read as relational rows and joined with
  normal tables (§2.2);
- files can be written/updated/deleted from inside SQL expressions
  and procedures (§2.1) — within the transactionality contract of §5;
- `COPY` and `read_parquet(...)` can use `fs9://` as a data source
  (§2.3);
- the SQL surface, the WebSocket surface, and the FUSE / `db9 fs cp`
  surface all resolve to the **same** tenant namespace. This is the
  cross-transport namespace invariant from design 29 §0; it is the
  load-bearing reason for several integration choices in §3 and §5.

What the integration explicitly does **not** provide:

- SQL-style transactional semantics over fs9 mutations (§5).
- Predicate/projection pushdown into `extensions.fs9(...)` table-
  function scans (§3.7).
- Range reads for `read_parquet('fs9://...')` (§3.6).
- IMMUTABLE/STABLE/VOLATILE classification on fs9 functions, with
  the correctness consequences in §3.7 / §2.5.
- TVF-side privilege gating (§8.0 — **KNOWN ISSUE**).

---

## 2. SQL Surface Area

Four entry shapes, with materially different privilege and behavior
contracts. See §8 for the full privilege story; the table below is
the surface-area summary.

### 2.0 Decision table — when to choose fs9 vs S3 vs neither

| Need | Use | Why |
|---|---|---|
| Read CSV/JSONL/Parquet as rows; join with tables | `extensions.fs9(...)` TVF | streaming via mpsc (§3.5); schema inference at PARSE (§4); no whole-file buffering |
| One-shot small text read inside a SQL expression / DEFAULT / PL/pgSQL body | `fs9_read` / `fs9_read_bytea` | whole file materialised in memory (`fs9.rs:144`); see §2.1 caps |
| Bulk import → table | `COPY t FROM 'fs9://...'` | privilege ordering correct (§2.3, §8.3) and emits `58030` / `42501` cleanly |
| Mutate a sealed file (`PackEntry` / `Object`) at an offset | none | rejected by design — §6.1 sealed-file contract |
| Stream binary blob > 100 MiB through SQL | none today | `MAX_BYTES_PER_FILE = 100 MiB` (`mod.rs:52`); use FUSE / `db9 fs cp` |
| Cross-tenant or unauthenticated read | none | tenant keyspace required (§7) |
| Long-lived publish/subscribe over file changes | not `fs9_events` alone | events can drop (§11; `redis_events.rs:314`) — combine with `fs9_stat` |

OPEN: when JuiceFS / S3 direct-presigned download is required
(large binary download bypassing SQL), the contract is
`FsBackend::prepare_download` — but neither this doc nor
`fs9_extension.md` documents user-visible SQL wrapping. Track if a
SQL surface is wanted.

### 2.1 Scalar functions

Registered in `src/sql/expr/functions/fs9.rs:50`. Gated by
`ensure_permissions()` at `fs9.rs:66-73` (superuser + backend
available). Permission check runs **before** any argument is read,
so path-existence probing by non-superusers is closed
(`fs9.rs:135-145`, `:147-169`, `:171-183`).

| Function | Signature | SQLSTATE today | Notes |
|---|---|---|---|
| `fs9_read(path)` | `TEXT → TEXT` | XX000 | strict UTF-8; reserves up to file size against `FS9_READ_BUDGET` (`fs9.rs:13`, `:142-144`) |
| `fs9_read_bytea(path)` | `TEXT → BYTEA` | XX000 | binary round-trip (`fs9.rs:326-340`) |
| `fs9_read_at(path, off, len)` | `→ TEXT` | XX000 | byte-window read, must be valid UTF-8 |
| `fs9_read_at_bytea(path, off, len)` | `→ BYTEA` | XX000 | byte-window read |
| `fs9_write(path, data)` | `TEXT \| BYTEA → INT64` | XX000 | full replace; promotes Inline → Object > `inline_max_bytes` (§6) |
| `fs9_write_at(path, off, data)` | `→ INT64` | XX000 | partial write; rejected on sealed (§6.1); cap `MAX_BYTES_PER_OFFSET_WRITE = 4 MiB` (`mod.rs:62`) |
| `fs9_append(path, data)` | `→ INT64` | XX000 | append; works on sealed files via delta/sidecar |
| `fs9_truncate(path, size)` | `→ BOOLEAN` | XX000 | rejected on sealed |
| `fs9_exists(path)` | `→ BOOLEAN` | XX000 | |
| `fs9_size(path)` | `→ INT64` | XX000 | |
| `fs9_mtime(path)` | `→ TEXT` | XX000 | RFC 3339 UTC |
| `fs9_remove(path, recursive?)` | `→ INT64` | XX000 | returns deletion count |
| `fs9_mkdir(path, recursive?)` | `→ BOOLEAN` | XX000 | |

**KNOWN ISSUE — scalar fs9 errors collapse to `XX000`.** All scalar
error paths use `anyhow!(...)` (e.g. `fs9.rs:33`, `:71`, `:124`,
`:159`, `:441`, `:472`). None downcasts to `SqlError` /
`StorageError`, so `sqlstate_for_executor_error`
(`src/protocol/handler/errors.rs:103`) falls through to `XX000`
(internal_error). User-induced conditions (oversize, bad UTF-8,
budget exhausted, permission denied) are indistinguishable from
genuine internal bugs on the wire. Contrast COPY, which emits clean
`58030` / `42501` (`copy/fs9.rs:20-22`, `:134-140`). See §5.6 for the
full SQLSTATE table.

**KNOWN ISSUE — path is absent from oversize/budget error payloads.**
`fs9.rs:124-128`, `:159-163`, `:441-449`, `:472`, `:33-39`
interpolate sizes but not the offending path, even though the path
argument is in scope. A row-by-row batch INSERT that fails on row 47
gives the caller no way to identify the failed row. Cheap fix.

**KNOWN ISSUE — `FS9_READ_BUDGET` (128 MiB, `fs9.rs:13`) is
process-global, not per-tenant.** A noisy tenant's read storm
starves every other tenant on the same db9-server. See §11
(operational mitigation).

Caps: file size 100 MiB (`MAX_BYTES_PER_FILE`, `mod.rs:52`); offset
write 4 MiB (`mod.rs:62`); concurrent read budget 128 MiB
process-wide (`fs9.rs:13`). The sibling user manual
`docs/fs9_extension.md` still cites 10 MiB — that doc is stale; this
doc is the current contract.

### 2.2 Table functions

Dispatched in `src/sql/executor/extensions.rs` (FS9, FS9_JG) and
`src/sql/executor/table_functions.rs` (FS9_EVENTS,
FS9_STORAGE_STATS).

| TVF | Purpose | Reference |
|---|---|---|
| `extensions.fs9(path [, named-params])` | Read files/dir/glob as rows; CSV/TSV/JSONL/text/Parquet decoders | dispatch `extensions.rs:409` |
| `extensions.fs9_jg(path, query)` | Server-side JSONL search via `jsongrep` DSL | dispatch `extensions.rs:589` |
| `fs9_events(since_id [, prefix [, limit]])` | Tail fs9 mutation event stream (Redis-backed) | dispatch `table_functions.rs:355`; impl `extensions/fs/notify.rs` |
| `fs9_storage_stats()` | Aggregate logical storage per tenant | dispatch `table_functions.rs:406`; impl `extensions/fs/stats_worker.rs` |

**KNOWN ISSUE — TVF privilege gap.** None of the four fs9 TVFs
checks `is_superuser()` in dispatch. Contrast `extensions.rs:399`
(HTTP TVF, correctly gated) with `:409` (FS9), `:589` (FS9_JG),
`table_functions.rs:355` (FS9_EVENTS), `:406` (FS9_STORAGE_STATS).
Any non-superuser role in a database with the `fs9` extension
installed can `SELECT * FROM extensions.fs9('/anything')` and read
any file in the tenant namespace. The `is_superuser` checks at
`extensions.rs:857`, `:971` gate `CREATE EXTENSION` (DDL only) and
do not substitute. See **§8.0** for the full security framing and
remediation status.

### 2.3 `fs9://` URLs

Recognised by `is_fs9_url` (`src/extensions/parquet/reader.rs:196`).
Accepted in:

```sql
COPY t FROM 'fs9://data/imports.csv'      WITH (FORMAT csv);
COPY t FROM 'fs9://data/imports.parquet'  WITH (FORMAT parquet);
SELECT * FROM read_parquet('fs9://snapshots/q3.parquet');
```

COPY routes through `src/protocol/handler/dynamic/copy/fs9.rs`;
`read_parquet` through `src/extensions/parquet/`. Both share
`FsBackend` (§3.2). COPY's privilege check is **file-then-table**
(`copy/fs9.rs:134-140` → `:151-166`) so non-superusers cannot probe
table existence via the error shape; see §8.3 for the
information-leak framing.

### 2.4 CREATE EXTENSION gating (current behavior)

| Entry shape | Runtime gate | Code |
|---|---|---|
| `CREATE EXTENSION fs9` | superuser | `extensions.rs:857` |
| `DROP EXTENSION fs9` | superuser | `extensions.rs:971` |
| Scalar `fs9_*` | superuser + backend-available | `fs9.rs:66-73` |
| `COPY FROM 'fs9://'` | superuser + INSERT priv (file-first) | `copy/fs9.rs:134-140` |
| `read_parquet('fs9://...')` | superuser before backend acquire | `parquet/reader.rs:42` |
| TVF `extensions.fs9(...)` etc. | **NONE** (see §8.0 KNOWN ISSUE) | `extensions.rs:409`, `:589`; `table_functions.rs:355`, `:406` |

The previous version of this doc claimed *"equivalent check inside
the TVF dispatch path"*; that is **not true** on master `9d8488d2`.
See §8.0.

### 2.5 Volatility / planner notes

**KNOWN ISSUE — fs9 functions have no volatility classification.**
The `SqlFn` type (`src/sql/expr/functions/mod.rs`) is
`fn(Vec<Value>) -> Result<Value>` with no purity metadata; grep over
`src/sql/expr/functions/fs9.rs` and `src/sql/types/registry/`
returns zero VOLATILE/STABLE/IMMUTABLE markers. The optimizer's
`push_filter_down` (`src/sql/optimizer/rewrite/mod.rs:74`) does not
inspect purity, so fs9 functions are treated as effectively
IMMUTABLE — worst case for correctness. Full code-path framing in
§3.7.

Concrete failure:

```sql
SELECT t.id FROM files_table t JOIN snapshots s ON ...
 WHERE fs9_exists(t.path);
```

If `fs9_exists(t.path)` is pushed past the join, fs9 RPCs fire for
every row of `files_table` regardless of join filtering. At typical
table sizes that blows through `FS9_READ_BUDGET` (128 MiB
process-global, §2.1) and starves other tenants; the query then
fails with budget-exceeded (XX000). Two `fs9_exists('/a')` calls in
one statement can return different answers (no statement snapshot,
§5.2), but the optimizer may CSE them.

Doc-level guidance until the optimizer is fixed: avoid fs9 scalars
in predicates over joins; prefer `extensions.fs9(...)` TVF or
materialise into a CTE first.

### 2.6 When to use scalar vs TVF

| Use | Choose | Why |
|---|---|---|
| Read a small file (< ~1 MiB) inside a projection / DEFAULT / PL/pgSQL body | scalar `fs9_read` | only shape supported in those contexts |
| Read a file as rows, join, aggregate | `extensions.fs9(...)` TVF | streaming via mpsc; schema inference (§4) |
| Glob over a directory tree | TVF | scalar cannot |
| Search inside JSONL files | `extensions.fs9_jg(...)` | server-side `jsongrep` DSL; streams matching records |
| File > ~1 MiB | TVF | scalar materialises the whole file in `Value::Text` / `Value::Bytes` (`fs9.rs:144`); easy to OOM at projection-level |
| Bind path as `$1` / `?` in a prepared statement | **scalar only** | TVF prefetch fails on parametric paths — see §4.1 |

---

## 3. Execution Architecture

### 3.1 Request flow for a statement that touches fs9

```
pgwire connection
  └── Parser (sqlparser)
        └── Analyzer (synchronous)
              └── catalog_prefetch
                    └── prepared_analysis.rs:118, 194 (PARSE-time)
                          └── catalog_prefetch::build_catalog_snapshot_inner
                                └── resolution.rs:583 fs::infer_table_function_schema  ← I/O at plan time
              └── Typed IR (AnalyzedQuery)
        └── Optimizer
              └── LogicalPlan → PhysicalPlan → BoxedOperator
                    └── TableFunctionScanOperator (mpsc-fed; §3.5)
        └── Executor
              ├── scalar fs9_* call sites
              │     └── SqlFsClient (src/extensions/fs/sql_client.rs:14)
              │           └── FsBackend (§3.2)
              └── table_function streaming producer
                    └── start_file_stream / start_glob_stream
                          / list_directory_entries
                          └── FsBackend
```

The Analyzer is synchronous, but fs9 schema inference is async (it
must `stat` / `read_file` to derive columns). The Analyzer therefore
relies on a **prefetch phase** keyed by
`table_function_key(name, args)`
(`src/sql/table_functions.rs:43`) computed from the *literal*
argument list, run before analysis to bind the schema. Implications
for prepared statements: §4.1.

### 3.2 The `FsBackend` trait

All four entry shapes ultimately call methods on
`trait FsBackend` (`src/extensions/fs/backend.rs:211`). Trait
methods: `stat`, `readdir`, `readdir_recursive`, `read_file`,
`read_file_at`, `read_file_stream`, `batch_inline_read`,
`write_file`, `write_file_at`, `append_file`, `truncate`, `remove`,
`remove_recursive`, `mkdir`, `rename`, `symlink`, `readlink`,
`chmod`, `batch_write_grouped`, `begin_write_stream`, plus the
multipart-upload set (`create_upload` / `presign_upload_part` /
`complete_upload` / `abort_upload`) and `prepare_download`.

Two concrete backends today, per tenant:

| Kind | Backend | Selected when |
|---|---|---|
| `Embedded` | `EmbeddedFsBackend` over `EmbeddedPageFs` in TiKV (`src/extensions/fs/embedded/`) | tenant has no `jfs_t_<id>` PD keyspace |
| `JuiceFs` | `GrpcFsBackend` via fs-plane (`src/extensions/fs/grpc/`) | PD reports `jfs_t_<id>` keyspace in state `ENABLED` |

Both are wrapped by `NormalizingFsBackend`
(`src/extensions/fs/normalizing.rs:129-133`) so path canonicalization
is identical regardless of backend. The wrapper applies
`to_fs9_canonical_path` (`src/extensions/fs/mod.rs:149`) inside
every method. COPY paths, scalar paths, and
`read_parquet('fs9://...')` paths therefore canonicalize the same
way; see §8.3 for the security consequence (no
pre-vs-post-canonicalization bypass).

The trait abstracts over two structurally different storage shapes
(PageFs full-TiKV-transactional vs JuiceFS-via-RPC). The capability
matrix it papers over is in §9.1.

### 3.3 Per-statement backend acquisition

Every SQL entry point reuses `acquire_statement_backend`
(`src/extensions/fs/backend.rs:951`). Behavior:

- On first call within a statement, probes PD to resolve the
  `TenantBackendKind` (`backend.rs:570`
  `resolve_tenant_backend_kind`), instantiates the backend, wraps it
  in `NormalizingFsBackend`, and stores it in the per-statement
  extension context (`ExtensionContext::cached_fs_backend`).
- Subsequent calls within the same statement return the cached
  handle.
- The cache is **statement-scoped, not process-scoped**. A new
  statement re-probes PD. This is intentional — keyspace lifecycle
  can change while the process lives (ENABLED → DISABLED on
  `db9 delete`), and a process-wide cache would mask teardown
  (`backend.rs:583-592`). It is also the routing-integrity gate
  whose failure mode is a security event — see §7.2 / §8.1.

This caching layer is **not a transaction handle**. It only
deduplicates backend construction; each backend method still opens
its own TiKV transaction (§5.1).

### 3.4 SQL ↔ async bridging for scalar functions

`SqlFn` is synchronous (the SQL evaluator runs scalars on a Tokio
worker thread, not in an `async fn`). Scalar fs9 functions bridge to
the async backend via `block_in_place` + `Handle::current().block_on`
(`src/sql/expr/functions/fs9.rs:105`):

```rust
fn run_async<T>(future: impl Future<Output = T>) -> T {
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(future)
    })
}
```

Safe because the SQL executor task runs on the multi-thread Tokio
runtime; `block_in_place` is the documented escape hatch there. The
runtime invariant — multi-thread runtime with `worker_threads >= 2`
— is load-bearing; see §11.6 for the failure mode when violated
and §5.7 for cancellation semantics (`block_on` does not propagate
Tokio cancellation).

### 3.5 Table-function streaming

The table function is fed into the operator pipeline as a
`TableFunctionScanOperator`
(`src/sql/operators/table_function.rs:11`). The operator has one
row source: an `mpsc::Receiver<Row>` fed by a producer task spawned
in `start_file_stream` / `start_glob_stream` /
`list_directory_entries`.

Channel depth and producer model:

- Row channel depth is **256** (`src/extensions/fs/file_stream.rs:84`,
  `:107`, `:130`; `glob_stream.rs:169`).
- Backpressure flows through the mpsc: when downstream operators
  block, the producer suspends on `tx.send(...).await`.
- The producer is detached via `tokio::spawn`. Early consumer close
  (LIMIT, error, client disconnect) is detected only at the next
  failed `tx.send(...).await` — so a stuck send can leak up to 256
  rows of decoded state before the producer notices and exits.
- For File mode, the decoder runs on a dedicated task; for Glob
  mode, files are scanned sequentially with a cumulative **100 MiB
  byte budget** (`MAX_TOTAL_BYTES`), after which remaining files are
  skipped and a server-side warning is logged (`fs9: bytes budget
  exhausted`, `table_function.rs:108`). The byte budget exists so
  `WHERE` clauses see complete rows from every scanned file, never
  a truncated tail row. **Silent-partial**: callers receive the
  rows already emitted with no error and no SQLSTATE warning. See
  §11.

`read_parquet('fs9://...')` does NOT use this streaming path — see
§3.6.

### 3.6 Range reads for `read_parquet('fs9://...')`

**KNOWN ISSUE.** `read_parquet('fs9://...')` reads the entire file
into a `Bytes` buffer in db9-server process memory before parquet
decoding begins.

Evidence:
- `src/extensions/parquet/reader.rs:46-49` —
  `backend.read_file(path, MAX_FS9_PARQUET_FILE_BYTES)` is awaited
  for the *whole* file.
- `src/extensions/parquet/fs9_reader.rs:14` —
  `MAX_FS9_PARQUET_FILE_BYTES = 100 * 1024 * 1024`.
- `src/extensions/parquet/fs9_reader.rs:26-44` — `Fs9ParquetReader`
  serves `get_bytes(range)` by slicing the in-memory `Bytes`. No
  range-read against the backend.

Consequences:

- A 90 MiB file with `SELECT col FROM read_parquet('fs9://x.parquet')
  WHERE pk=42 LIMIT 1` reads all 90 MiB into process memory.
- Files larger than 100 MiB cannot be queried via `read_parquet` at
  all, even when storable as `Object` (S3-backed).
- Memory cost ≈ `file_size × concurrent_queries`. The 128 MiB
  global read budget (`fs9.rs:13`) is shared with scalar `fs9_read`,
  so a single 100 MiB parquet query nearly drains it.

Contrast with DuckDB, Trino, ClickHouse `s3()`: all three issue a
footer range-read first, then row-group-byte-range reads informed
by predicate/projection pushdown. db9's *HTTP* parquet reader
(`src/extensions/parquet/http_reader.rs:215`) already implements
range reads. The fs9 path does not, even though
`FsBackend::read_file_at` exists (`backend.rs:433`).

**To fix**: wire `backend.read_file_at` into
`Fs9ParquetReader::get_bytes`. Tracked as an open item; until then,
for analytic workloads on parquet larger than ~100 MiB, prefer
HTTPS/S3 URLs.

### 3.7 Optimizer awareness of fs9

**KNOWN ISSUE — two parts.**

**(a) No predicate/projection pushdown into `extensions.fs9(...)`.**
`grep -rn` of `src/sql/optimizer/` for `fs9` or `extensions::fs`
returns nothing. The optimizer has no rewrite or cost model that
knows about the TVF. Consequences:

- CSV/JSONL decoders materialize ALL columns even if the SELECT
  lists one. Projection pushdown is missing.
- WHERE clauses run in a downstream Filter operator, after every
  row has been decoded and pushed through the 256-deep mpsc.
- For parquet, the parquet builder's standard column projection
  (`ParquetRecordBatchStreamBuilder::with_projection`) is never
  wired up.

**(b) Scalar fs9 functions carry no volatility class — the optimizer
assumes purity it doesn't have.** See §2.5 for the SQL surface and
the concrete failure mode. The underlying code paths:

- `SqlFn` signature (`src/sql/expr/functions/mod.rs`) is
  `fn(Vec<Value>) -> Result<Value>` with no IMMUTABLE/STABLE/VOLATILE
  field.
- Predicate pushdown (`src/sql/optimizer/rewrite/mod.rs:74`,
  `push_filter_down`) pushes WHERE predicates past joins
  unconditionally. It does not inspect function purity.
- fs9 reads are **VOLATILE** per §5.2: two evaluations in the same
  statement may observe different filesystem snapshots. The
  push-past-join rewrite assumes at least STABLE semantics.

**Recommendation in code**: tag every fs9 function VOLATILE; teach
`push_filter_down` to refuse to push VOLATILE predicates past
joins. Requires extending `SqlFn` to carry function metadata.
Tracked as a separate code-fix issue.

### 3.8 Backend acquisition error taxonomy

Every fs9 SQL entry point routes through `acquire_statement_backend`
(`src/extensions/fs/backend.rs:951`), which on first call probes PD
via `resolve_tenant_backend_kind` (`backend.rs:570`). All errors
below surface as raw `anyhow::Error` and therefore as SQLSTATE
`XX000` to the client (§5.6, §11).

| Error string (substring) | Source | Class | Retry posture | Alerting |
|---|---|---|---|---|
| `PD keyspace probe ... HTTP {status}` | `backend.rs:727`, `:737` | Transient (PD slow/unreachable; mTLS material expired) | Retry with backoff, jittered. Affects **every** first-fs9-call per statement (§11; env invariants in §11.6). | Page on >0 rate for >1 min — every statement that touches fs9 fails until PD heals. **Also a security signal** (§7.2). |
| `JuiceFS keyspace ... is in state '{X}' (expected ENABLED) ... refusing to route to the embedded backend` | `backend.rs:586-592` | Permanent for this tenant (PD reports teardown in progress) | Do not retry. Tenant is gone. | Per-tenant signal; investigate db9-backend lifecycle. |
| `fs9: tenant '{ks}' is JuiceFS-backed and requires an authenticated principal` | `backend.rs:909-915` | Permanent (caller path did not propagate auth) | Do not retry. | Indicates a wiring bug in the SQL entry path. Page. |
| `fs9: tenant keyspace '{ks}' lacks the db9_tenant_ prefix; cannot derive tid claim` | `backend.rs:916-923` | Permanent (malformed tenant context) | Do not retry. | Page; likely pgwire username parse bug. |
| `fs9: TiKV storage backend not available` | `fs9.rs:67` | Transient at startup; permanent otherwise | Retry once after backoff. | Should never appear post-startup; page. |
| `fs9: permission denied (superuser required)` | `fs9.rs:71` | Permanent (caller is not superuser) | Do not retry. | Per-tenant audit signal; correlate with role. |

Operationally critical: the PD-transient and PD-teardown cases
share the same `anyhow` envelope and surface as the same SQLSTATE.
Apps cannot classify without string-matching the message. KNOWN
ISSUE, tracked in §11.

---

## 4. Schema Inference

`extensions.fs9(...)` has a dynamic schema that depends on the path
argument. Inference runs in `infer_table_function_schema`
(`src/extensions/fs/table_function.rs:8`) during analyzer prefetch.

| Mode | Schema |
|---|---|
| Directory | fixed: `(path TEXT, type TEXT, size INT64, mode INT64, mtime TEXT)` |
| File / CSV / TSV | first row = header by default; all columns `TEXT` |
| File / JSONL | fixed: `(_line_number INT, line JSONB, _path TEXT)` |
| File / Parquet | inferred from parquet schema (feature-gated) |
| File / text | fixed: `(_line_number INT, line TEXT, _path TEXT)` |
| Glob | schema of first matched file; subsequent files must match (CSV column-name equality enforced) |

Implications:
- Schema inference reads the file (CSV: header line; Parquet: footer)
  at planning time. This is real I/O during analysis.
- Glob heterogeneity is rejected with `fs9 glob schema mismatch: ...`
  (PR #cb1a2d63).

### 4.1 Prepared statements and the extended query protocol

Schema inference runs at **PARSE time, not BIND/EXECUTE**. Verified:

- `src/sql/executor/core/prepared_analysis.rs:118`, `:194` call
  `build_catalog_snapshot` during the PREPARE pipeline.
- `src/sql/executor/core/catalog_prefetch/resolution.rs:583` awaits
  `fs::infer_table_function_schema(...)` and stores via
  `snapshot.add_table_function(&call.key, schema)`.
- `src/sql/executor/core/dispatch/prepared.rs:826` destructures
  `PreparedExec::AnalyzedQuery { analyzed, locks, .. }` and goes
  straight to `execute_via_optimizer` — no re-analysis, no
  re-prefetch.
- Drift detection at `prepared.rs:598` (`first_schema_drift_on_txn`)
  tracks **SQL table** versions only; fs9 file mtimes/sizes are not
  in the dependency list.

Two consequences with sharp app impact:

1. **Literal-path prepared statements freeze schema.**
   `PREPARE p AS SELECT * FROM extensions.fs9('/a.csv')` binds the
   schema at PARSE. If `/a.csv` later gains/loses/reorders columns,
   subsequent EXECUTEs return rows shaped by the OLD schema with no
   invalidation hook. Downstream type errors surface as confusing
   runtime failures.

2. **Parametric-path prepared statements are silently broken.**
   `src/sql/table_functions.rs:118` uses
   `eval_const_ast_expr(path_expr).ok()?` to extract the path; a
   parameter placeholder (`$1` / `?`) returns `None`, no schema
   entry is cached, analysis fails with an XX000-class error. So
   `PREPARE p AS SELECT * FROM extensions.fs9($1)` does not work.
   ORMs that auto-prepare every SELECT (asyncpg default; pgjdbc with
   `prepareThreshold > 0`) **cannot use the TVF form with a bind
   parameter at all.**

   Workarounds: (a) use scalar `fs9_*` (per-row evaluation, no PARSE
   prefetch); (b) interpolate the path client-side as a literal and
   forgo prepared-statement caching for that query.

OPEN: should db9 implement deferred schema inference for parametric
paths (defer first-EXECUTE prefetch and freeze on first bind)?
Cheap DX win; non-trivial cache-invalidation question.

---

## 5. Transactionality Contract (read carefully)

This is the most often-misunderstood part of the integration. The
contract is *structural*, not a feature gap.

### 5.0 Why fs9 does not enlist in the SQL transaction

A reader familiar with PostgreSQL's `lo_*` large-object API will
expect fs9 writes to participate in the SQL session transaction.
They cannot, for three architectural reasons:

1. **Two-keyspace problem.** SQL data lives under `d_{db_id}_*` keys
   in the tenant's keyspace; fs9 metadata lives under `_fs_*` keys
   (embedded) or in a separate `jfs_t_<id>` PD keyspace served by
   fs-plane (JuiceFS). Enlisting fs9 writes in the SQL session's
   TiKV transaction would require either (a) two-keyspace 2PC,
   which nothing in the stack provides, or (b) a SQL-only carve-out
   that creates a namespace divergent from WS/FUSE/CLI — forbidden
   by the cross-transport namespace invariant of design 29 §0.

2. **fs-plane is a separate process.** For JuiceFS-backed tenants,
   writes hit `GrpcFsBackend` (`src/extensions/fs/grpc/`), which
   RPCs into the fs-plane proxy. The proxy holds its own JuiceFS
   client and metadata transactions. There is no API by which
   db9-server's SQL transaction could enlist fs-plane's writes —
   that would require a 2PC protocol fs-plane does not implement.

3. **Per-call independent transactions.**
   `EmbeddedFsBackend::write_file`
   (`src/extensions/fs/embedded/pagefs/write_impl.rs:33`) opens its
   own `txn = self.begin().await?`, mutates inode + blob + parent
   directory, and commits before returning. This is by design
   (cross-surface symmetry from §3.2). The SQL session's TiKV
   transaction client is a different instance, with a different
   lifecycle.

Design 29 §0.1 records the explicit non-goal: *"do not change raw
`FsBackend` semantics into a SQL-specific contract."* The integration
doc previously surfaced only the *consequence* (§5.2); this section
is the *reason*.

### 5.1 What fs9 guarantees

- **Per-operation atomicity.** Each `FsBackend` method opens an
  independent TiKV optimistic transaction (embedded) or single
  fs-plane RPC (JuiceFS) and commits before returning.
- **Grouped per-directory atomicity** via `batch_write_grouped`
  (`backend.rs:419`). When the backend reports
  `supports_batch_write_atomic() == true` (`backend.rs:406-408`,
  currently embedded only), `batch_write_grouped` commits each
  parent-directory group in a single TiKV transaction, with
  bounded internal retries on `txn_conflict`. **Cross-backend gap**:
  JuiceFS backend returns `false` and rejects grouped writes.
- **Cross-surface namespace consistency** (design 29 §0): SQL, WS,
  FUSE, and `db9 fs cp` all see the same directory tree for a given
  tenant. There is no SQL-only namespace overlay.

### 5.2 What fs9 does NOT guarantee

- **No participation in the SQL session transaction.** A
  `BEGIN ... ROLLBACK` block does not undo `SELECT fs9_write(...)`
  performed inside it. Once the call returns, its side effect is
  committed and visible to other sessions. See §5.0 for *why*.

- **Savepoints do not roll back fs9 writes either.**
  `SAVEPOINT s; SELECT fs9_write(...); ROLLBACK TO s` leaves the
  write in place. The `SavepointManager`
  (`src/txn/savepoints.rs:12-31`) operates on a SQL-side undo log
  of in-session data writes (`savepoints.rs:104` rollback drains
  that undo); there is no fs9 participant in the savepoint stack
  and no fs9 undo callback. App developers reaching for SAVEPOINT
  as a workaround for the lack of enlistment will get silent
  incorrectness; see §5.5 for the patterns that *do* work.

- **`PREPARE TRANSACTION` is a future incompatibility hazard.**
  db9 currently has no `PREPARE TRANSACTION` support
  (`grep -rn "PREPARE TRANSACTION\|PreparedTransaction\|pg_prepared_xacts" src/`
  returns zero hits at `9d8488d2`). If/when XA-style distributed
  transactions are added, fs9 writes will conflict with the 2PC
  promise — the fs9 write commits at SELECT time, before the
  PREPARE protocol sees it. Flag this constraint now so future XA
  work does not propose enlisting fs9.

- **No statement-level snapshot consistency for fs9 reads.** Each
  read opens its own fresh `begin_read()` TiKV snapshot
  (`src/extensions/fs/embedded/pagefs/read_impl.rs:5`). Two fs9
  calls in the same SQL statement (e.g.
  `SELECT fs9_size('/a'), fs9_read('/a')`) may observe different
  versions of `/a` if another writer commits between them.

  **PG `lo_*` mental model**: inside a transaction, every `lo_read`
  sees that transaction's earlier `lo_write` calls because both run
  inside the same backend's snapshot. fs9 does NOT. Two consecutive
  `fs9_read` calls in one statement are more analogous to two
  separate `dblink` connections than to PG's intra-transaction file
  access.

- **No read-your-own-writes against uncommitted SQL state.** fs9
  reads see the latest *fs9-committed* state, not the SQL session's
  snapshot ts.

- **No cross-resource 2PC.**
  `INSERT INTO t ... ; SELECT fs9_write(...)` inside a `BEGIN` block
  has two independent commit points. The table write commits with
  the session transaction; the fs9 write commits immediately.

### 5.3 Failure modes integrators should plan for

| Scenario | Result |
|---|---|
| `BEGIN; SELECT fs9_write('/a','x'); ROLLBACK;` | `/a` contains `'x'` permanently; ROLLBACK only affects SQL-table mutations |
| `SAVEPOINT s; SELECT fs9_write('/a','x'); ROLLBACK TO s;` | `/a` contains `'x'` permanently; SAVEPOINT does not participate (§5.2) |
| Two sessions concurrently `fs9_write` the same path | Last committer wins (full-replace semantics); inode generation bumps; no merge |
| `fs9_write_at` on a `PackEntry` or `Object` (embedded backend) | Hard error: *"partial mutation is not supported for sealed files"* — see §6.1 |
| `fs9_write_at` on a 1 MiB file, embedded vs JuiceFS tenant | **Backend-asymmetry footgun.** Embedded: fails (file is sealed). JuiceFS: succeeds. See §6.2. |
| `fs9_append` on a sealed file (embedded) | Succeeds via append-delta (`_fs_AD` blocks for Object, sidecar tail for Pack) up to the cap |
| Glob over heterogeneous CSV headers | Statement-level error (`fs9 glob schema mismatch`) |
| Cumulative glob payload > 100 MiB | Glob stops mid-traversal; warning logged; rows already emitted remain valid. **Silent-partial** — see §11 |
| Concurrent reads exceeding `FS9_READ_BUDGET` (128 MiB process-global) | New `fs9_read` calls fail with `concurrent read budget exceeded`. Existing reads complete normally. See §11 (runbook). |
| `SELECT t.id FROM t JOIN u ... WHERE fs9_exists(t.path)` | Optimizer may push the fs9 predicate below the join (§3.7) → thousands-fold fs9 RPC amplification. **KNOWN ISSUE**. |

### 5.4 Where the contract is enforced in code

| Concern | File |
|---|---|
| Per-op TiKV txn boundaries | `src/extensions/fs/embedded/pagefs/write_impl.rs:33` (write); `read_impl.rs:5` (read) |
| Per-statement backend acquisition (NOT a txn handle) | `src/extensions/fs/backend.rs:951` `acquire_statement_backend` |
| Backend kind probe (fail-closed on non-ENABLED keyspace) | `src/extensions/fs/backend.rs:570` `resolve_tenant_backend_kind`; cross-link §7.2 |
| Path canonicalization at trait boundary | `src/extensions/fs/normalizing.rs:129-133`; helper at `src/extensions/fs/mod.rs:149`. **No bypass** between COPY/scalar/`read_parquet` — §8.3 |
| SqlFsClient as the only SQL→backend boundary | `src/extensions/fs/sql_client.rs:14` (header is the contract) |
| Sealed-file mutation rejection (embedded only) | `src/extensions/fs/embedded/pagefs/ops_impl.rs`; §6.1 |
| SQL session transaction (unaffected by fs9) | `src/txn/state.rs`; savepoint manager `src/txn/savepoints.rs:12-31` |

### 5.5 Patterns cookbook for app code

fs9 mutations do not enlist in the SQL session transaction (§5.2).
The closest the architecture gives app code is the following three
patterns, in order of robustness.

**Content-addressed paths (most robust):**

```
1. compute h = sha256(content)
2. fs9_write('/blobs/<h>', content)      -- side effect; idempotent
3. INSERT INTO docs (id, blob_hash) VALUES (?, h)
4. COMMIT
```

Both ends commute on retry: re-writing `/blobs/<h>` with identical
content is a no-op for the application; re-inserting under
`ON CONFLICT DO NOTHING` is a no-op for the table. If COMMIT fails,
the blob is orphan (garbage-collectable by a separate sweep keyed on
"hashes not referenced by any row"); the application can safely
retry the whole sequence.

**Staging-then-rename (soft pattern):**

```
fs9_write('/staging/<session_id>/x', ...)
... other SQL work ...
-- rename '/staging/<session_id>/x' → '/published/x'   (OPEN, see below)
COMMIT
```

Caveat: rename also commits eagerly. If COMMIT fails after the
rename succeeds, `/published/x` has the new content but the SQL row
that references it is absent. This is "closer to atomic than
nothing" but does **not** give 2PC semantics.

OPEN: `fs9_rename` is not registered as a SQL scalar today
(`src/sql/expr/functions/fs9.rs:50-64`). The `FsBackend` trait has
`rename` (`src/extensions/fs/backend.rs:211+`) and WS/FUSE/CLI can
issue it, but the SQL surface cannot. Until a wrapper lands, the
staging-then-rename pattern is only executable from WS/FUSE/CLI —
SQL apps that need it must fall back to compensating-delete or
content-addressed.

**Compensating delete (last resort):**

App-level try/catch issues `SELECT fs9_remove(...)` if the SQL
commit fails. Requires the app to correlate client-side state with
what was written. Vulnerable to (a) crash between write and commit —
the compensation never runs; (b) double-execution under retry —
the delete may race with a successful retry's write.

**Ordering rules:**

- **Insert-then-write** is preferable when the SQL row is the source
  of truth (e.g. `INSERT` allocates an id that becomes part of the
  path).
- **Write-then-insert** is preferable for content-addressed schemes
  (the hash is determinate before the row exists).
- `SAVEPOINT s; SELECT fs9_write(...); ROLLBACK TO s;` does NOT undo
  the fs9 write (`src/txn/savepoints.rs:12-31`; see §5.2).

**Triggers and DEFAULT expressions:**

OPEN: should fs9 mutating scalars be permitted in trigger bodies or
column DEFAULTs? The code permits it today, but a trigger that
calls `fs9_write` and then RAISEs leaves the file behind — same root
cause as §5.2, sharper consequence because triggers can fire on
cascading DML. Recommend documenting as a discouraged pattern and
adding a lint, not blocking by SQLSTATE.

### 5.5b Cross-surface consistency under concurrent writes

The cross-transport namespace invariant (design 29 §0) guarantees
SQL/WS/FUSE see the same directory tree. It does **not** specify
read visibility timing in the presence of concurrent writes from
another surface. What the code gives you:

- **Scalar fs9 reads** (`fs9_read`, `fs9_size`, `fs9_exists`,
  `fs9_mtime`): each opens a fresh handle for one operation
  (`Open → Pread → Close` for FsPlane per design 33 §3.3 table; a
  fresh TiKV read txn per call for embedded). Close-to-open
  consistency holds — a fs9 read issued after a WS/FUSE writer's
  flush sees the new bytes.

- **Streaming table function** (`extensions.fs9(...)`,
  `read_parquet('fs9://...')`): the read handle is opened once and
  held for the duration of decoding
  (`src/extensions/fs/file_stream.rs:69`). Concurrent writes from
  another surface during the scan have **undefined visibility** for
  rows already buffered downstream; whether the in-flight
  `read_file_stream` reader observes the new bytes depends on
  backend internals not contracted by this layer.

If a query needs a consistent view across multiple fs9 reads in one
statement, materialize via a single call (one
`extensions.fs9(...)` invocation, then reuse its rows). Multiple
scalar `fs9_*` calls in one statement are not consistent with each
other; see §5.2.

### 5.6 SQLSTATE table (current behavior)

Today, scalar fs9 surfaces collapse to `XX000` regardless of cause.
This table is the current behavior, not the desired contract.

| Entry point | Failure | SQLSTATE today | Desired | Retry class (see §3.8) |
|---|---|---|---|---|
| Scalar fs9_* | permission denied | XX000 (`errors.rs:103`) | 42501 | fail-permanent |
| Scalar fs9_* | backend unavailable | XX000 | 58030 | retry-after-backoff |
| Scalar fs9_read* | read budget exceeded | XX000 (`fs9.rs:33-39`) | 54000 | retry-after-backoff |
| Scalar fs9_write | file too large | XX000 (`fs9.rs:159-163`) | 54000 | fail-permanent |
| Scalar fs9_write_at | offset write > 4 MiB | XX000 (`fs9.rs:441-449`) | 54000 | fail-permanent |
| Scalar fs9_write_at / truncate | sealed file | XX000 | 0A000 (feature_not_supported) | fail-permanent |
| Scalar fs9_read | not valid UTF-8 | XX000 | 22021 (character_not_in_repertoire) | fail-permanent |
| TVF analyzer | glob schema mismatch | XX000 | 42804 (datatype_mismatch) | fail-permanent |
| TVF executor | bytes budget exhausted (glob) | (no error; log only) | n/a | silent-partial — see §11 |
| COPY FROM fs9:// | non-superuser | 42501 (`copy/fs9.rs:135-139`) | 42501 | fail-permanent |
| COPY FROM fs9:// | backend IO error | 58030 (`copy/fs9.rs:20-22`) | 58030 | retry-after-backoff |
| `fs9_events` | event dropped on Redis flush failure | (no error; events lost) | n/a | dropped-no-error — see §11 |
| Backend acquire | PD transient HTTP failure | XX000 | 58030 | retry-after-backoff |
| Backend acquire | tenant teardown (non-ENABLED) | XX000 (`backend.rs:586`) | 57P03 (cannot_connect_now) | fail-permanent |

**KNOWN ISSUE — XX000 ambiguity.** App-side retry policies cannot
distinguish user errors from internal bugs based on SQLSTATE alone.
Workaround until remediated: pgwire message strings carry the
`fs9_*: ` prefix (`fs9.rs` uses `anyhow!("fs9_X: ...")`
consistently), so detectors can match on message prefix in addition
to SQLSTATE. Fragile but the only signal today.

### 5.7 Extended query semantics — cancellation and `block_on`

Scalar fs9 functions bridge sync `SqlFn` to async backend via
`block_in_place(|| Handle::current().block_on(...))` at
`src/sql/expr/functions/fs9.rs:105-107`.

OPEN — **Cancellation semantics.** `block_on` does not propagate
Tokio cancellation. A client `Ctrl-C` mid-`fs9_write` does not
interrupt the in-flight backend call; the cancel takes effect at
the next await point in the SQL executor, after the fs9 side effect
has already committed. Apps assuming PG-like cancel-clean semantics
will be surprised. Recommend documenting as a known footgun until
a structured cancellation hook is wired through the `FsBackend`
trait.

OPEN — **Multi-thread runtime invariant.** `block_in_place` panics
on a current-thread Tokio runtime. The contract is enforced only by
the runtime configuration in `src/main.rs` (§11.6); if anything
ever spawns a db9 SQL executor on a current-thread runtime, every
fs9 scalar panics. Worth a debug-assert at the `run_async` call
site.

OPEN — **TVF parametric paths**: see §4.1.

---

## 6. Storage Classes and the Sealed-File Mutation Contract

fs9 v2 publishes three storage classes for regular files (design
`29_fs9_v2_*.md` §0), with a fourth shape introduced by fs-plane.

| Class | Backing | Sealed? | Mutating ops on embedded | Mutating ops on JuiceFS |
|---|---|---|---|---|
| `Inline` (`InlineBlob`) | TiKV value alongside inode | no | all (`write_file`, `write_file_at`, `append`, `truncate`) | n/a (no Inline class on JuiceFS — see §6.2) |
| `Pack` (`PackEntry`) | slice of an immutable S3 bundle | yes | `write_file` (full replace), `append` (sidecar tail) | n/a |
| `Object` | dedicated S3 object | yes | `write_file` (full replace), `append` (delta blocks, capped) | (FsPlane equivalent — see §6.2) |
| FsPlane `DataRef` | JuiceFS volume (via fs-plane) | no | n/a | all, no inline ceiling (design `33_fs_plane_*.md` §3.5) |

Promotion rules (embedded only):

- `fs9_write` of a file larger than `inline_max_bytes` (config;
  default ~64 KiB) auto-promotes Inline → Object (PR #2417, commit
  `bd64bf6a`).
- `fs9_append` overflow on an inline file promotes Inline → Object
  (`03c1ce92`).
- Promotion is staged then committed atomically; staging is cleaned
  on failure (`d72cd5e6`, `61f3aab6`).

### 6.1 Sealed-file mutation contract (embedded)

`fs9_write_at` / `fs9_truncate` on a sealed file (`PackEntry` or
`Object`) returns a clear error rather than silently doing
read-modify-write. Enforced at
`src/extensions/fs/embedded/pagefs/ops_impl.rs` against `DataRef`.
Design intent: design 29 §0.1 *"No SQL mutation carve-out for
sealed files in Phase 1."*

Tests guarding this on embedded:
`fs9_write_at_preserves_sealed_file_contract`,
`fs9_truncate_preserves_sealed_file_contract`,
`fs9_append_works_on_sealed_files`,
`fs9_write_full_replace_succeeds_on_sealed_file`
(`src/sql/expr/functions/fs9.rs:980+`).

### 6.2 Backend-asymmetry warning

The v1 doc claimed the sealed-file contract is *"uniformly exposed
at the SQL surface."* That overstated parity. JuiceFS-backed tenants
have **no Inline class and no inline-size ceiling**: FsPlane
`DataRef` accepts `WriteAt` at any size (design
`33_fs_plane_*.md` §3.5: *"Unlike `InlineBlob`, FsPlane has no
64 KB ceiling"*). Consequence:

| SQL call | Embedded tenant (Inline → Object @ ~64 KiB) | JuiceFS tenant (FsPlane DataRef) |
|---|---|---|
| `fs9_write_at('/p', 0, <1 MiB>)` on a 1 MiB file | fails: *"partial mutation is not supported for sealed files"* once promoted | succeeds |
| `fs9_truncate('/p', 1024)` on a 1 MiB file | fails: same | succeeds |

The same SQL is portable in shape but not in outcome. Apps that
target both backend types cannot rely on `fs9_write_at` /
`fs9_truncate` succeeding on any file >64 KiB without first checking
`stat()`'s `storage` field. There is no SQL-side opt-in that forces
uniform behavior.

**Recommendation:** apps that need cross-backend portability should
`stat` first and branch on `storage`, or use `fs9_write`
(full-replace) which succeeds uniformly on both backends. KNOWN
ISSUE.

---

## 7. Multi-Tenancy

Per CLAUDE.md "Multi-tenancy Invariant", all fs9 state is
keyspace-isolated. Per-backend layout:

| Backend | Metadata | Data | Routed by |
|---|---|---|---|
| Embedded | TiKV under `db9_tenant_<id>`, prefix `_fs_*` (families `_fs_S` / `_fs_AI` / `_fs_I` / `_fs_D` / `_fs_B` / `_fs_P` / `_fs_L` / `_fs_M` / `_fs_T` / `_fs_O` / `_fs_AD`, `embedded/keys.rs:7-20`) | TiKV inline + S3 (Object/Pack) | absence of `jfs_t_<id>` PD keyspace |
| JuiceFS | PD keyspace `jfs_t_<id>` (`extensions/fs/mod.rs:67`, `jfs_volume_id`) | configured S3 bucket via fs-plane gRPC (`grpc/client.rs:162`) | `jfs_t_<id>` keyspace ENABLED in PD |

Tenant is parsed from the pgwire username
(`src/protocol/handler/tenant.rs::parse_tenant_username`); a single
db9-server process can host both backends, decided per statement
(§3.3, §3.8). Process-wide caches scoped to fs9: connection pools,
config, and the fs-plane JWT cache (§8.7 / §11 KNOWN ISSUE).

### 7.1 Operator runbook — "which backend is tenant X on?"

There is no in-band way today. The recipe:

1. Get the tenant ID from the pgwire username
   (`src/protocol/handler/tenant.rs::parse_tenant_username`).
2. `kubectl exec` into the db9-server pod (or any host with
   `PD_ENDPOINTS` reachability + the right mTLS material).
3. Query PD's keyspace API at `/pd/api/v2/keyspaces/jfs_t_<id>`
   with the mTLS material from `TIKV_CA_PATH` / `TIKV_CERT_PATH` /
   `TIKV_KEY_PATH` (`backend.rs:631-697`).
4. Interpret the `state` field per `backend.rs:583-592`:
   - absent → embedded
   - `ENABLED` → JuiceFS
   - any other state → tenant in teardown; fs9 calls fail-closed

OPEN: add a `SELECT fs9_backend_kind()` SQL function so operators
have an in-band lever. Today the answer requires PD HTTP + mTLS
plumbing.

### 7.2 Misroute as a cross-tenant integrity event

If `resolve_tenant_backend_kind` (`backend.rs:570-593`) ever returns
the wrong state — PD bug, mTLS material pointing at the wrong PD
cluster, env-var swap — the consequence is silent. Verified against
master: there is **no cross-check** at the write path.
`resolve_tenant_backend_kind` is consumed once at
`backend.rs:825-849` and then discarded; neither embedded
(`embedded/keys.rs:7-20`) nor gRPC (`grpc/client.rs:162`) asserts the
other backend has no state for this tenant.

Worst-case sequence: PD bug routes JFS tenant T to embedded for one
statement → `_fs_*` keys land under `db9_tenant_T` → PD heals →
JuiceFS reads succeed for the customer → shadow `_fs_*` data
persists indefinitely (background maintenance, `pagefs.rs:1772`,
only fires on the next misroute).

`PD_ENDPOINTS`, `TIKV_CA_PATH`, `TIKV_CERT_PATH`, `TIKV_KEY_PATH`
are therefore security-boundary config. Drift detection is a
security control — threat-model home is §8.1. This section owns
only operational detection: PD probe error rate
(`backend.rs:737`) is the only external signal of a misroute window
today.

OPEN: add a structural invariant — at the embedded backend's first
write per statement, re-check PD and refuse if PD reports ENABLED.
Mirror check at gRPC backend's first call.

---

## 8. Privileges and Authorization

### 8.0 Corrected privilege statement (v1 §8 was wrong here)

| Surface | Gate | Anchor |
|---|---|---|
| Scalar `fs9_*` functions | `ensure_permissions()` → `is_superuser()` | `src/sql/expr/functions/fs9.rs:66-73` |
| `COPY ... FROM 'fs9://...'` (CSV/TEXT) | `session.is_superuser()` before table-privilege | `src/protocol/handler/dynamic/copy/fs9.rs:134-140` |
| `COPY ... FROM 'fs9://...'` (Parquet) | `session.is_superuser()` before table-privilege | `src/protocol/handler/dynamic/copy/fs9.rs:611-617` |
| `read_parquet('fs9://...')` | `is_superuser()` before backend acquire | `src/extensions/parquet/reader.rs:42` |
| `extensions.fs9(...)` TVF | **NONE — KNOWN ISSUE** | `src/sql/executor/extensions.rs:409-587` |
| `extensions.fs9_jg(...)` TVF | **NONE — KNOWN ISSUE** | `src/sql/executor/extensions.rs:589-820` |
| `fs9_events(...)` TVF | **NONE — KNOWN ISSUE** | `src/sql/executor/table_functions.rs:355-405` |
| `fs9_storage_stats()` TVF | **NONE — KNOWN ISSUE** | `src/sql/executor/table_functions.rs:406-411` |

**KNOWN ISSUE: TVF privilege gap.** The four TVFs above have no
`is_superuser` / `Privilege::` / `require_table_privilege` /
`ext_permission_denied` check on their dispatch path. The `HTTP_*`
TVF immediately above the FS9 dispatch at `extensions.rs:399` *does*
gate — the omission is structurally visible.

Severity: **intra-tenant privilege escalation**. The TVFs use
`self.tenant_keyspace()` (`table_functions.rs:397`, `:408`), so a
tenant-A user cannot reach tenant-B via TVFs. But any role in a
database with `CREATE EXTENSION fs9` enabled can read every file in
their own tenant via `SELECT * FROM extensions.fs9('/secret/path')`.
The sibling user manual `docs/fs9_extension.md:207` ("Permission
required | Superuser only") is also wrong and must be corrected in
lockstep.

OPEN: is the missing gate an intentional relaxation, or an
oversight that should be fixed with `ensure_permissions()` calls
matching the HTTP TVF pattern at `extensions.rs:399`?

### 8.1 Trust model and crown-jewel secrets

**PG comparison (three planes, three verdicts):**

| Plane | PG `pg_read_server_files` | fs9 |
|---|---|---|
| Data | host filesystem (host certs, /etc/passwd, configs) | TiKV-isolated per-tenant namespace — **strict win** |
| Mint | n/a (no signing infrastructure) | process-scoped via `DB9_AUTH9_SERVICE_API_KEY` — **no PG analog** (neutral) |
| Code-path | requires explicit grant of `pg_read_server_files` predefined role | scalars + COPY: `is_superuser` only (less granular than PG predefined-role); TVFs: ungated — **strict loss vs PG today** |

**Crown-jewel secrets:**

- `DB9_AUTH9_SERVICE_API_KEY` — `mint()` at
  `src/auth/fs_plane_token.rs:259-304` takes `tenant_id` as a
  parameter and constructs the `tid` claim from it; no per-request
  verification ties the claim to the requesting principal. A
  compromise (process memory disclosure suffices — no code
  execution needed) can mint `aud="fs-plane"` rw tokens for ANY
  tenant on the fs-plane.
- `PD_ENDPOINTS` env (`src/extensions/fs/backend.rs:603-606`) +
  `TIKV_CA_PATH` / `TIKV_CERT_PATH` / `TIKV_KEY_PATH` — routing
  integrity boundary. Misroute → silent cross-tenant shadow
  namespace (see §7.2).
- TiKV API V2 keyspace prefix — applied **client-side** by
  `TikvStore`, not enforced by TiKV (design `33_fs_plane_*.md` §6.1:
  *"client-side key prefix encoding, not server-enforced access
  control"*). The CLAUDE.md "Multi-tenancy Invariant" is enforced
  by code discipline, not by storage-layer ACL.

### 8.2 Audit story and gap

| Backend | Authoritative "who wrote what" | Anchor |
|---|---|---|
| JuiceFS | auth9 `jwt.signed` audit chain (per-mint `service_id`, `aud`, claims) | `docs/design/fs9_auth9_direct_mint.md:158-166` |
| Embedded | **none** — pgwire query log only (if enabled) | KNOWN ISSUE |

`fs9_events(...)` TVF deliberately omits actor identity — the schema
(`src/extensions/fs/notify.rs:464-479`) carries `stream_id`,
`event_type`, `path`, `old_path`, `inode`, `generation`, `is_dir`,
`size`, `timestamp` only. It is a **content-change feed, not an
audit log.** Apps that need actor attribution must JOIN
`fs9_events` with the pgwire / SD-wrapper SQL audit log offline.

Under SECURITY DEFINER elevation (§8.5): the fs-plane JWT's `usr`
claim names the **caller**, not the wrapper owner
(`src/extensions/context.rs:312-324`, test at `:493-510`). There is
no claim that says *"this rw was granted via SD"*. Reconstructing
whether a write was elevated requires both fs9-side logs and the
db9-server SD-wrapper definition.

OPEN: planned audit chain for embedded backend, or "rely on pgwire
query log" is the accepted answer?

### 8.3 Privilege-check ordering invariant (no path-existence probing)

Both COPY and scalar fs9 functions deny non-superusers **before**
parsing or touching the `path` argument:

- COPY (CSV/TEXT): `src/protocol/handler/dynamic/copy/fs9.rs:134-140`
  — superuser check first, table-privilege second, file-read third.
- COPY (Parquet): `src/protocol/handler/dynamic/copy/fs9.rs:611-617`
  — same order.
- Scalars: `src/sql/expr/functions/fs9.rs:135-145` (`fs9_read`),
  `:147-169` (`fs9_write`), `:171-183` (`fs9_exists`),
  `:185-198` (`fs9_size`) — `ensure_permissions()?` first,
  `expect_text_arg` second, backend call third.

Non-superusers cannot use these surfaces to probe path existence by
error-message shape. (TVFs do not enforce permissions at all — see
§8.0 — so the invariant does not apply there.)

**PR #2179 reframe.** PR #2179 is an **information-leak defense**
(CWE-209 family), not an access-control primitive. The AC was
already in place; what PR #2179 added is error-message-shape
uniformity so callers cannot use `42P01 (undefined_table)` vs
`42501 (insufficient_privilege)` to enumerate which tables exist.

**Path canonicalization invariant (positive finding).** COPY
(`copy/fs9.rs:278-289`), `read_parquet`
(`parquet/reader.rs:34-50`), and scalar fs9 all route through
`acquire_statement_backend` → `NormalizingFsBackend` wrapper, which
applies `to_fs9_canonical_path`
(`src/extensions/fs/mod.rs:149-169`) before delegating to the leaf
backend. An attacker cannot exploit pre-vs-post-canonicalization
string differences (e.g. submit `/foo/../etc/passwd` hoping a
privilege check sees the raw string while the backend canonicalizes);
every entry point sees the same canonical path at the `FsBackend`
trait boundary.

**Path canonicalization scope.** `to_fs9_canonical_path` enforces
only: `..` rejection, `.` segment collapse, empty-segment collapse,
leading `/`. It does **not** reject NUL bytes, apply Unicode NFC
normalization, enforce path-length limits, or filter control
characters. fs9 paths are byte-opaque to db9-server; backend-side
validation (`validatePath` on JuiceFS) is the final word.

### 8.4 Role identity vs capability (corrects v1 §8 line 412)

v1 §8 claimed *"_db9_sys_readonly sessions are forced to read-only
fs9 access regardless of role"* — this is **wrong in two ways**:

1. **Capability wins over name.**
   `src/auth/fs_plane_token.rs:229-237` (`fs_plane_access_for`): a
   superuser-capable session gets `Fs9Access::ReadWrite` regardless
   of role name. Test at `:546-554` confirms a superuser named
   `_db9_sys_readonly` still gets `:rw`.
2. **Embedded has no capability layer.** `_db9_sys_readonly` is
   used **exclusively** for fs-plane JWT-scope minting
   (`fs_plane_token.rs:242 SYS_READONLY_ROLE`). On embedded
   backends, the only gate is `is_superuser()` — a non-superuser
   session named `_db9_sys_readonly` cannot use fs9 at all (it fails
   `ensure_permissions()`).

**Corrected statement:** *"On JuiceFS-backed tenants, a non-superuser
session authenticated as `_db9_sys_readonly` is constrained to
fs-plane scope `:r`. A superuser-capable session (regardless of role
name) is `:rw`. Embedded tenants have no capability layer — only
the superuser check applies."*

`_db9_sys_readonly` is **not a regular PG role** in the user table
— it is a string constant the auth layer recognises
(`fs_plane_token.rs:242`), a service-account convention shared with
fs9 v2.

### 8.5 SECURITY DEFINER as the documented escape hatch

`SECURITY DEFINER` PL/pgSQL functions inherit the definer's
privileges for fs9 calls **iff the definer is a superuser**.
Mechanism: `src/sql/plpgsql/executor.rs:54-66` calls
`enter_security_definer_superuser()` only when
`func_def.owner.is_superuser`. CTX wiring at
`src/extensions/context.rs:200-203` makes `is_superuser()` return
`ctx.is_superuser || ctx.security_definer_superuser.get()`.
Regression test: `tests/306_fs9_security_definer.sql`.

**The SD wrapper IS the documented escape hatch** for letting
non-superuser apps reach fs9. It is also a **privilege boundary on
par with PG's `pg_read_server_files`** (same confused-deputy
discipline — distinct from the three-plane security comparison in
§8.1): this is the default failure mode. Wrapper authors MUST validate every argument
(path allowlist, content shape, size) before calling fs9 — an
unvalidated `path` turns the wrapper into a *"read/write anywhere on
this tenant's fs9"* capability.

**Non-superuser-owner footgun.** `CREATE FUNCTION ... SECURITY
DEFINER ... OWNER non_super` is a no-op for fs9 — the wrapper does
not grant any new capability (SD elevates to *owner's* privileges,
and the owner has no fs9 privilege to elevate to). The call fails
at runtime with `"fs9: permission denied (superuser required)"` and
**SQLSTATE `XX000`** (not `42501` — see §8.6). Fix: transfer
ownership to a superuser, then `GRANT EXECUTE` to the app role.

**Audit replay caveat under SD.** The fs-plane JWT's `usr` carries
the caller's role (e.g. `alice`), and `scp` is `:rw` if elevation
fired. fs9-side audit attributes the write to the caller — there is
no claim that says *"this rw was granted via SD."* App-level audit
must JOIN `fs9_events` (or fs-plane access logs) with the
db9-server SQL audit log to reconstruct whether the write was
elevated.

### 8.6 SQLSTATEs for fs9 permission denial today

| Surface | SQLSTATE | Message | Anchor |
|---|---|---|---|
| COPY | `42501` | `"permission denied to COPY from a file"` | `src/protocol/handler/dynamic/copy/fs9.rs:135-139` |
| Scalar `fs9_*` | **`XX000`** (KNOWN ISSUE) | `"fs9: permission denied (superuser required)"` | `src/sql/expr/functions/fs9.rs:71` + `src/protocol/handler/errors.rs:103` |
| TVFs | n/a — no gate (§8.0) | — | — |

**KNOWN ISSUE: scalar fs9 SQLSTATE.** `ensure_permissions()` returns
plain `anyhow!()`. `sqlstate_for_executor_error`
(`src/protocol/handler/errors.rs:77-104`) cannot downcast it to any
recognised error type, so it falls through to `XX000` on line 103.
Apps writing `WHERE sqlstate = '42501'` insufficient-privilege
detectors will miss scalar fs9 denials. Fix: return
`SqlError::InsufficientPrivilege`-equivalent.

### 8.7 Token cache lifecycle — KNOWN ISSUE

The fs-plane JWT cache (`src/auth/fs_plane_token.rs:138-177`) is
process-wide (`OnceLock<Arc<Fs9PlaneTokenCache>>` at
`src/extensions/fs/backend.rs:939-945`) and has **no invalidation
API**. Public surface: `new`, `lookup_fresh`, `store` (private),
`mint_or_reuse`. Grep across `auth/`, `extensions/fs/`,
`extensions/fs/grpc/` finds zero `invalidate` / `remove` / `clear`
/ `purge_tenant` references.

TTL window: `FS_PLANE_TTL_SECS = 900` (`fs_plane_token.rs:50`),
`REFRESH_LEAD = 60` (`:82`). A cached token is usable for up to
**~14 minutes** after issuance, returned to anyone matching
`(tenant_id, role, access)`.

**fs-plane does NOT validate volume state per RPC.** Design
`33_fs_plane_*.md` §0.2 line 29: *"thin gRPC proxy ... no
business-logic state — no generation tracking, no idempotency cache,
no tenant metadata."* fs-plane trusts the JWT's `tid` claim
end-to-end. Net enforcement boundary: **JWT expiry alone.**

Two consequences:

1. **Tenant recycle.** If `tenant_id` `acme` is deleted at T+0 and
   re-created within ~14min, the new admin's first SQL fs9 call
   returns the OLD rw JWT from cache. fs-plane honors it.
2. **Mid-statement teardown.** A long-running
   `COPY FROM 'fs9://big.parquet'` started before keyspace flip to
   DISABLED continues issuing gRPC calls under a still-valid JWT
   until expiry (~14min ceiling). Doc v1 §11 called this
   *"acceptable for JuiceFS"* — unjustified given fs-plane
   statelessness.

OPEN: does db9-backend forbid `tenant_id` reuse within ≥14min of
teardown? PD state machine
`absent → ENABLED → DISABLED → ARCHIVED → TOMBSTONE`
(`backend.rs:576-592` comment) suggests a tombstone lifecycle that
may gate; the policy is **cross-team** with db9-backend.

Fix paths (lead to choose): (a) add
`Fs9PlaneTokenCache::invalidate_tenant(tenant_id)` and wire to a
teardown signal source; (b) re-key the cache to
`(tenant_id, generation)` so recycle is safe by construction;
(c) accept the ~14min window as documented risk.

---

## 9. Version History (abridged)

| Era | Backend | Key commits |
|---|---|---|
| v0 (Feb 2026) | `LocalFsBackend` over host `tokio::fs`, read-only TVF | `eec09f47` |
| v1 (Mar 2026) | `Fs9HttpBackend` over HTTP to an external fs9-server process | `a785300b`, `73ad4577` |
| v1 + WS | adds WebSocket protocol for non-SQL clients (CLI, FUSE) | `23f500d1`; design `26_*` |
| v2 (late Mar 2026) | embed `EmbeddedPageFs` directly in tenant's TiKV keyspace; remove v0/v1 backends | `91f6a943`, `6161679d` |
| v2 + tiering (Apr 2026) | Inline + Pack + Object storage classes; promote-on-overflow; S3 backend; format version 4 | design `29_*`; PRs `#2077..#2417` |
| fs-plane / JuiceFS (Apr–May 2026) | dual backend (embedded vs JuiceFS); per-tenant PD probe; gRPC client to fs-plane proxy; auth9 direct mint | design `33_*`, `fs9_auth9_direct_mint.md`; commits `6816a506`, `9d8488d2` |

Current head (`9d8488d2`): v2 gRPC backend on `fsplane.v2` restored
with auth9-direct fs-plane JWT minting. The dual-backend integration
was reverted and restored more than once (`4418eae3`, `18f81230`,
`2e33c752`, `9d8488d2`) — read recent history before assuming a
particular backend is wired in.

### 9.1 Why the dual-backend integration is fragile

Three reverts in ~two months on the same architectural decision is
a signal worth surfacing. Structural causes:

- **Per-statement PD probe on the fast path.**
  `resolve_tenant_backend_kind` (`src/extensions/fs/backend.rs:570`)
  is an HTTP call to PD on the first fs9 touch of every statement.
  PD outage → fs9 outage even for embedded tenants whose backend
  type is structurally fixed.
- **Fail-closed on non-ENABLED keyspace** (`backend.rs:583-592`) is
  correct but means a JuiceFS teardown bug surfaces as universal
  fs9 failure for that tenant, not just for writes.
- **One trait, two semantics.** `FsBackend` papers over capability
  differences. The matrix the trait hides:

  | Capability | Embedded | JuiceFS (fs-plane) |
  |---|---|---|
  | `supports_batch_write_atomic` (`backend.rs:406-408`) | true | false |
  | `supports_presigned` (`backend.rs:412-414`) | varies (S3-bound) | per-op via fs-plane |
  | `fs9_write_at` on >64 KiB file | rejected (sealed — §6.1) | accepted (no ceiling — §6.2) |
  | Append-delta mechanism (`_fs_AD`) | embedded only | n/a (JuiceFS has its own Append+Flock) |
  | Mutation atomicity granularity | TiKV optimistic txn | single gRPC RPC + JuiceFS internal metadata-engine txn (design `33_fs_plane_*.md` §3) |

  See §6 for the SQL-surface consequences (same call, different
  outcome by backend).

### 9.2 The "in-process" decision is undocumented and now structurally asymmetric

Commit `6161679d` (Feb 25, 2026) deleted `Fs9HttpBackend` and
`LocalFsBackend` with this entire rationale:

> Removed LocalFsBackend and Fs9HttpBackend implementations from
> backend.rs
> Removed all remote/local dual code paths from fs9.rs SQL
> functions
> Removed fs9_events extension handling from executor
> Removed fs9_proxy and fs9_client from cloud-admin-portal
> Removed jsonwebtoken dependency
> Simplified to single embedded TiKV PageFS backend
> All 2111 tests pass

No design rationale captured. No performance numbers. No
v1-problem-X. Compare to
`docs/design/33_fs_plane_juicefs_data_engine.md` §0.1, which is
explicit about *why* per-tenant FUSE mounts were rejected — that is
the standard a v1→v2 transition commit should meet.

**More importantly**: the "in-process is simpler" defense of v2 is
now structurally inconsistent with the present architecture.
fs-plane (design 33) reintroduces a separate gRPC sidecar process
for JuiceFS-backed tenants (`src/extensions/fs/grpc/`). v2's
in-process design therefore only applies to *embedded* tenants;
JuiceFS tenants are back to two processes. The architecture is no
longer a uniform principle — it is an embedded-only choice with a
sidecar exception.

OPEN: reconstruct the v1→v2 rationale via interview with the
original authors, or admit it's lost. Either is honest; the present
state (silent gloss) is not.

---

## 10. Quick Reference

| Task | File |
|---|---|
| Add a scalar fs9 function | `src/sql/expr/functions/fs9.rs`; register in `register()` (`:50`) |
| Add a table function | `src/sql/executor/extensions.rs` dispatch + `src/extensions/fs/` impl |
| Change schema inference | `src/extensions/fs/table_function.rs` (`infer_table_function_schema`) |
| Change streaming behavior | `src/extensions/fs/file_stream.rs`, `glob_stream.rs`, `directory.rs` |
| Add a backend method | `trait FsBackend` in `src/extensions/fs/backend.rs:211`; implement in `embedded/` and `grpc/` |
| Change path canonicalization | `src/extensions/fs/normalizing.rs:129-133` + `mod.rs:149` |
| `COPY FROM 'fs9://...'` | `src/protocol/handler/dynamic/copy/fs9.rs` |
| `read_parquet('fs9://...')` | `src/extensions/parquet/reader.rs:34-50` + `fs9_reader.rs` (whole-file buffered — §3.6) |
| Backend routing rules | `src/extensions/fs/backend.rs:570` (`resolve_tenant_backend_kind`) |
| Statement-scoped backend | `src/extensions/fs/backend.rs:951` (`acquire_statement_backend`) |
| Per-tenant JWT minting | `src/auth/fs_plane_token.rs` (no invalidation — §8.7) |
| Event stream / `fs9_events` | `src/extensions/fs/notify.rs`, `src/extensions/fs/redis_events.rs` |
| Predicate pushdown rewrite (does NOT inspect fs9 purity — §3.7) | `src/sql/optimizer/rewrite/mod.rs:74` (`push_filter_down`) |
| Savepoint manager (does NOT participate in fs9 — §5.2) | `src/txn/savepoints.rs:12-31` |
| Sibling user manual (parts are stale — §11) | `docs/fs9_extension.md` |

---

## 11. Open / Sharp Edges

KNOWN ISSUES that the doc must be honest about. Each line lists a
hazard, its anchor, and (where applicable) the on-call workaround.

- **Scalar fs9 errors collapse to `XX000`**
  (`src/protocol/handler/errors.rs:103`). All scalar `fs9_*` errors
  are raw `anyhow!` (`fs9.rs:33`, `:71`, `:124`, `:159`, `:441`,
  `:472`) and fall through to the generic-internal SQLSTATE.
  `COPY FROM 'fs9://'` uses `58030` / `42501` (`copy/fs9.rs:20-22`).
  Alerts that key on `XX000` cannot distinguish user-induced fs9
  errors from genuine server bugs. See §5.6 for the per-message
  SQLSTATE table.
- **fs9 size/budget error messages omit the path argument**
  (`fs9.rs:33`, `:124`, `:159`, `:441`, `:472`). Path is in scope at
  every call site but is never formatted into the error. A batch
  INSERT that trips the budget on row 47 surfaces with no row
  identifier. Cheap fix; until it lands the runbook needs
  slow-query-log correlation by tenant + timestamp.
- **`fs9_events` may drop events.** Events are queued in an
  in-process mpsc and flushed in batches to Redis. On flush failure
  they are **dropped, not requeued** (`redis_events.rs:308-317`).
  Gaps appear as contiguous sequence to consumers — there is no
  persisted seq number — so loss is undetectable from the read side.
  Authoritative state must come from `fs9_stat` / `fs9_read`, not
  from the event tail.
- **`Fs9PlaneTokenCache` has no invalidation hook**
  (`src/auth/fs_plane_token.rs:138-177`, `backend.rs:939-945`).
  Public API is `new` / `lookup_fresh` / `store` / `mint_or_reuse`
  only; no `invalidate` / `remove` / `clear` / `purge_tenant`. A
  token minted for tenant T continues serving for up to ~14 min
  after T is deleted (`FS_PLANE_TTL_SECS = 900` at
  `fs_plane_token.rs:50` minus `REFRESH_LEAD = 60` at `:82`).
  fs-plane trusts the `tid` claim end-to-end (design 33 §0.2 line
  29). JWT expiry is the only mechanism that catches a stale token;
  there is no server-side cross-check. Mid-statement teardown
  enforcement for JuiceFS reduces to this same mechanism. See §8.7
  for the threat model; the operational consequence is: **only
  on-call lever is a process restart.**
- **`read_parquet('fs9://...')` reads the whole file into memory**
  (`src/extensions/parquet/fs9_reader.rs:14-44`). 100 MiB hard cap
  means Object-class files >100 MiB are storable but not queryable.
  Memory cost = file_size × concurrent_queries. See §3.6 for the
  fix plan.
- **`FS9_READ_BUDGET` is process-global, not per-tenant**
  (`fs9.rs:13-14`, single `AtomicUsize`). One noisy tenant can
  starve `fs9_read` for every other tenant on the pod. Until
  per-tenant budget exists, the on-call mitigation is to pause the
  offending role at pgwire (`ALTER ROLE … NOLOGIN`).
- **Glob byte budget is silent-partial.** When cumulative glob
  payload exceeds 100 MiB, scanning stops, a `tracing::warn!` is
  logged (`src/extensions/fs/table_function.rs:108`,
  `glob_stream.rs:177`, `:284`), and the caller receives the
  partial rows already emitted with no error and no SQLSTATE
  warning.
- **Per-statement backend cache survives mid-statement teardown.**
  A long-running COPY against a JuiceFS tenant continues against
  the bound backend even if PD flips the keyspace to DISABLED.
  Acceptable for embedded; for JuiceFS, see §8.7.
- **No fs9-side rollback hook for SQL transactions.** By design
  (§5), but easy to forget. Code that needs durable cross-resource
  atomicity must build it explicitly (staging writes under a
  per-transaction prefix and renaming on COMMIT, etc.; §5.5).
- **`block_in_place` requires multi-thread runtime.** Scalar fs9
  functions assume `Runtime::new_multi_thread()` (`main.rs:197`)
  with `worker_threads >= 2` (`main.rs:214`). Below that, fs9
  scalar calls will stall the async runtime — see §11.6.
- **Sibling doc `docs/fs9_extension.md` is stale.** It cites a
  10 MB per-file cap; the runtime cap is 100 MiB
  (`MAX_BYTES_PER_FILE`, `src/extensions/fs/mod.rs:52`). The
  user-facing manual must be re-validated against this doc before
  shipping.
- **Redis is a hard boot dependency** — the doc has long implied
  fs9 is an optional `CREATE EXTENSION` add-on. It is not at the
  process level: `init_redis_client().await?` (`main.rs:599-601`)
  fails the whole process if Redis is unreachable. SQL with zero
  fs9 references is blocked behind Redis availability.

OPEN: the required S3 bucket lifecycle policy must be documented
elsewhere (Helm chart or ops repo). Today it is implicit; if a
bucket is misconfigured, abandoned multipart parts accumulate
forever and incur S3 charges
(`src/extensions/fs/embedded/pagefs/read_impl.rs:808-908`).

### 11.5 Observability gap

fs9 has effectively **no Prometheus instrumentation today**. A grep
of `src/extensions/fs/` finds exactly one metric:
`db9_upload_sha256_seconds` (`src/extensions/fs/ws/mod.rs:921`).
All other "monitoring" is `tracing::{info,warn,error}!` log lines.

Specific gaps:

| What's missing | Code site | Today's signal |
|---|---|---|
| Glob truncation rate | `table_function.rs:108`, `glob_stream.rs:177`, `:284` | `warn!` only |
| Read-budget rejection rate | `fs9.rs:33-39` | error to caller; no counter |
| Backend resolution (embedded vs gRPC, per stmt) | `backend.rs:825-849` | no metric, no log |
| Redis event queue depth | `redis_events.rs:261` (`event_queue_depth()`) | function exists, **not wired to `metrics::gauge!`** |
| GC backoff state, orphan-inode count | `pagefs.rs:1781` (`consecutive_maintenance_failures`) | private, per-task local; not exported |
| Stats worker scan duration / staleness | `stats_worker.rs:88-103`, `:110` | info-log only |
| PD probe error rate | `backend.rs:737` | error to caller; no counter — **security signal per §7.2** |
| fs9 op latency by backend kind | — | no histogram; "is JuiceFS slower than embedded?" cannot be answered |

Until those land, alerts must key on log-line substrings:

| Substring | Meaning | Anchor |
|---|---|---|
| `fs9: bytes budget exhausted` | silent-partial result; query truncated | `table_function.rs:108` |
| `fs9_read: concurrent read budget exceeded` | process-global budget hit; correlate slow-query logs to find the noisy tenant | `fs9.rs:33-39` |
| `fs9_redis: event queue depth` | channel building up; check Redis health | `redis_events.rs:220-224` |
| `fs9 background maintenance failed` | per-tenant GC backing off | `pagefs.rs:1812` |
| `PD keyspace probe ... HTTP` | routing oracle unhealthy → §7.2 security signal | `backend.rs:727` |

### 11.6 Runtime config invariants

| Knob | Required value / default | Why | Citation |
|---|---|---|---|
| `tokio` runtime flavor | `multi_thread` | `block_in_place` in scalar fs9 (`fs9.rs:105-107`) panics on `current_thread` | `src/main.rs:197` |
| `worker_threads` | `>= 2` | Below 2, `block_in_place` stalls the only worker, freezing all async tasks on the pod | `main.rs:214`, `resolve_tokio_worker_threads` |
| `REDIS_URL` | required at boot | `init_redis_client().await?` fails the whole process if Redis is unreachable | `main.rs:599` |
| `FS9_GC_INTERVAL_SECS` | default 30 | Background maintenance cadence per-fs instance | `config.rs:20`, `:155` |
| `FS9_GC_INITIAL_JITTER_MS` | default 5 000 | Spread initial GC start across instances | `config.rs:21`, `:159` |
| `FS9_GC_MAX_BACKOFF_SECS` | default 600 | Cap on exponential backoff after failures | `config.rs:22`, `:162` |
| `FS9_STATS_REFRESH_INTERVAL_SECS` | default 60 (floor 5) | Cached storage stats freshness | `stats_worker.rs:62-67` |
| `FS9_NOTIFY_RING_CAPACITY` | default 10 000 | In-process event ring size; overflow surfaces via `oldest_seq` / `newest_seq` / `overflow` | `notify.rs:110-113` |
| `PD_ENDPOINTS`, `TIKV_CA_PATH`, `TIKV_CERT_PATH`, `TIKV_KEY_PATH` | required for PD probes | **Security-boundary config** per §7.2 / §8.1; drift is a security incident, not just ops drift | `backend.rs:603-643` |

OPEN: most of these are env-only today (no SQL `SHOW` surface). An
operator cannot tune what they cannot see; add a system TVF.
