# fs9 Extension

`fs9` is a built-in extension table function for db9-server that enables querying the server's local filesystem directly via SQL. It supports directory listing, single-file reading, and glob-based multi-file matching, with decoders for CSV, TSV, JSONL, and raw text formats.

## 1. Overview

- **Functionality**: Exposes filesystem data as SQL tables.
- **Permissions**: Superuser only.
- **Schema**: Functions live under the `extensions` schema, invoked as `extensions.fs9(...)`.
- **Installation**: Requires `CREATE EXTENSION fs9;` to enable.

## 2. Local Deployment / Quick Start

To use the `fs9` extension, you need a running db9-server environment.

### Quick Start Steps

```bash
# 1. Start a TiKV cluster (API v2 mode required for keyspace support)
uv run scripts/tikv_admin.py start --name dev --persistent

# 2. Build db9-server (release mode recommended)
cargo build --release

# 3. Start db9-server
PD_ENDPOINTS=127.0.0.1:2379 PG_PORT=5433 target/release/db9-server

# 4. Connect with psql
PGPASSWORD=admin psql -h 127.0.0.1 -p 5433 -U admin -d postgres
```

### Command Reference

| Operation | Command |
|-----------|---------|
| Start TiKV | `uv run scripts/tikv_admin.py start --name dev --persistent` |
| Stop TiKV | `uv run scripts/tikv_admin.py stop --name dev` |
| List clusters | `uv run scripts/tikv_admin.py list` |
| Start db9-server | `PD_ENDPOINTS=127.0.0.1:2379 PG_PORT=5433 cargo run --release` |
| Connect | `PGPASSWORD=admin psql -h 127.0.0.1 -p 5433 -U admin -d postgres` |

**Default credentials**: username `admin`, password `admin`.

## 3. Enabling / Disabling the Extension

The extension must be installed before use:

```sql
CREATE EXTENSION fs9;    -- Enable (superuser only)
DROP EXTENSION fs9;      -- Disable
```

## 4. Three Operating Modes

`fs9` automatically detects the operating mode from the path argument:

1. **Directory mode**: Path ends with `/`.
2. **Glob mode**: Path contains wildcards (`*`, `?`, `[`).
3. **File mode**: Everything else is treated as a single file read.

## 5. Directory Listing

List files and subdirectories in a given directory.

```sql
SELECT path, type, size, mode, mtime
FROM extensions.fs9('/data/')
ORDER BY path;
```

**Column descriptions**:
- `path` (TEXT): Full path to the entry.
- `type` (TEXT): Either `"file"` or `"dir"`.
- `size` (INT64): File size in bytes (0 for directories).
- `mode` (INT64): Unix permission mode (e.g., 33188 represents 0644).
- `mtime` (TEXT): Last modification time in RFC 3339 format.

**Example with exclude**:

```sql
-- Exclude hidden entries in directory listing
SELECT path, type, size
FROM extensions.fs9('/project/', recursive => true, exclude => '.*')
ORDER BY path;

-- Exclude multiple patterns (comma-separated)
SELECT path, type
FROM extensions.fs9('/data/', exclude => '*test*,*backup*')
ORDER BY path;
```

## 6. Reading Files

### CSV

Auto-detected by `.csv` extension. The first row is treated as column headers by default. All column values are `TEXT` (no type inference).

```sql
SELECT name, age, city FROM extensions.fs9('/data/users.csv') ORDER BY name;
```
- Supports quoted fields containing commas and newlines.
- No type inference; all data is returned as text.

### TSV

Auto-detected by `.tsv` extension. Delimiter is automatically set to tab (`\t`).

```sql
SELECT id, value FROM extensions.fs9('/data/report.tsv') ORDER BY id;
```

### JSONL (JSON Lines)

Auto-detected by `.jsonl` or `.ndjson` extension.

```sql
-- Basic query
SELECT _line_number, line FROM extensions.fs9('/logs/app.jsonl') ORDER BY _line_number;

-- Extract fields with json_extract
SELECT json_extract(line, '$.level') AS level,
       json_extract(line, '$.message') AS message
FROM extensions.fs9('/logs/app.jsonl')
WHERE json_extract(line, '$.level') = '"ERROR"';
```
- **Fixed schema**: `(_line_number INT, line JSONB, _path TEXT)`.
- Invalid JSON lines are silently skipped.

### Raw Text

Any unrecognized format falls back to line-by-line text reading.

```sql
SELECT _line_number, line FROM extensions.fs9('/etc/hosts') ORDER BY _line_number;
```
- **Schema**: `(_line_number INT, line TEXT, _path TEXT)`.
- Can be forced explicitly with `format => 'text'`.

## 7. Glob Multi-File Matching

Use wildcards to read multiple files in a single query.

```sql
-- Read all CSV files in a directory
SELECT _path, product, amount
FROM extensions.fs9('/data/sales/*.csv')
ORDER BY _path, product;

-- Recursive matching with **
SELECT _path, _line_number, line
FROM extensions.fs9('/logs/**/*.jsonl')
ORDER BY _path, _line_number;

-- Empty match returns 0 rows (no error)
SELECT * FROM extensions.fs9('/data/nonexistent-*.csv');

-- Exclude temp files from glob
SELECT _path, level, message
FROM extensions.fs9('/logs/*.jsonl', exclude => '*.tmp')
ORDER BY _path;

-- Exclude multiple patterns (comma-separated)
SELECT _path, _line_number, line
FROM extensions.fs9('/data/**/*.csv', exclude => '*test*,*backup*')
ORDER BY _path, _line_number;
```
- The `_path` column identifies which file each row came from.
- Result schema is derived from the first matched file.

## 8. Named Parameters

`fs9` supports named parameters to customize reading behavior.

```sql
-- Force a specific format
SELECT * FROM extensions.fs9('/data/raw.dat', format => 'csv');

-- Custom delimiter with no header row
SELECT col_0, col_1 FROM extensions.fs9('/data/raw.dat', format => 'csv', delimiter => '|', header => false);

-- Recursive directory listing
SELECT * FROM extensions.fs9('/data/', recursive => true);
```

**Parameter reference**:

| Parameter | Type | Description | Default |
|-----------|------|-------------|---------|
| `format` | TEXT | Force format: `'csv'`, `'tsv'`, `'jsonl'`, `'text'` | Auto-detected from file extension |
| `delimiter` | TEXT | CSV delimiter (single character) | `,` (TSV defaults to `\t`) |
| `header` | BOOLEAN | Whether the CSV file has a header row | `true` |
| `recursive` | BOOLEAN | Recursively list subdirectories (directory mode only) | `false` |
| `exclude` | TEXT | Glob pattern(s) to exclude from results. Supports comma-separated patterns (e.g., `'*.tmp,*.bak'`). Works in glob and directory modes. | None |

## 9. Limits and Security

To ensure system stability, `fs9` enforces the following limits:

| Limit | Value |
|-------|-------|
| Max file size | 10 MB |
| Max total bytes per glob query | 100 MB |
| Max glob traversal files | 10,000 |
| Max directory entries (recursive) | 100,000 |
| Max glob recursion depth | 10 levels |
| Permission required | Superuser only |

### How Limits Work

- **Per-file limit (10 MB)**: Individual files larger than 10 MB are rejected with an error.
- **Total bytes budget (100 MB)**: In glob mode, fs9 reads matched files until the cumulative bytes read reaches 100 MB. Remaining files are skipped and a server-side warning is logged. This ensures that SQL `WHERE` clauses see complete data from all scanned files — no partial row truncation occurs.
- **Glob traversal (10,000 files)**: The glob pattern expansion walks at most 10,000 filesystem entries to find matching files. This prevents runaway directory walks.
- **Directory listing (100,000 entries)**: Recursive directory listing returns at most 100,000 entries. If exceeded, a server-side warning is logged.

## 10. Architecture

`fs9` uses a backend abstraction to allow future extensibility.

```
FsBackend (Trait)
  |-- LocalFsBackend    <- Current: reads local disk via tokio::fs
  |-- Fs9HttpBackend    <- Future: HTTP calls to a remote fs9 server
```

**Design**:
- **FsBackend trait**: Defines four core methods: `stat`, `readdir`, `read_file`, `exists`.
- **Decoupled**: Decoders and glob logic are independent of the backend implementation.

**Source layout**:

| File | Purpose |
|------|---------|
| `src/extensions/fs/mod.rs` | Entry point, mode enum, execution routing |
| `src/extensions/fs/backend.rs` | FsBackend trait and LocalFsBackend implementation |
| `src/extensions/fs/decoders.rs` | CSV, JSONL, text, and directory decoders |
| `src/extensions/fs/glob.rs` | Glob pattern expansion |

## 11. Running Tests

### Unit Tests

Approximately 45 tests covering the backend, decoders, and glob modules.

```bash
cargo test extensions::fs
```

### Integration Tests

Requires a running db9-server server with a TiKV cluster.

```bash
# Create test fixtures
python3 tests/160_fs9_basic_load.py --port 5433 --user admin --password admin

# Run SQL integration tests
python3 scripts/integration_test.py --dsn "postgres://admin:admin@127.0.0.1:5433/postgres" \
    tests/160_fs9_basic.sql \
    tests/161_fs9_csv.sql \
    tests/162_fs9_jsonl.sql \
    tests/163_fs9_glob.sql
```
