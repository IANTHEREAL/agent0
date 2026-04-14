# List of Expressions for Pushdown

When `db9-server` reads data from DB9 Cop running in `cloud-storage-engine` / TiKV, it tries to push down some expressions, including function calls and operators, for storage-side evaluation. This reduces transferred data and offloads per-row computation from `db9-server`.

This page lists the expressions that are currently eligible for pushdown.

Notes:
- This page is an expression-level whitelist, not a guarantee that every query shape will be fully pushed down. `EXPLAIN` is the authoritative way to see what was actually pushed for a given query.
- Aggregate functions currently stay local on the exact DB9 Cop surface described here.
- Statement-stable temporal functions such as `NOW()` are not emitted as raw remote function calls. `db9-server` materializes them once per statement and then pushes the resulting constant.

## Supported expressions for pushdown to DB9 Cop

| Expression Type | Operations |
| --- | --- |
| Logical operators | `AND`, `OR`, `NOT` |
| Comparison operators | `=`, `<>`, `<`, `<=`, `>`, `>=`, `IS NULL`, `IS NOT NULL`, `IS DISTINCT FROM`, `IS NOT DISTINCT FROM` |
| Predicate operators | `BETWEEN`, `NOT BETWEEN`, `IN (...)`, `NOT IN (...)`, `IS TRUE`, `IS NOT TRUE`, `IS FALSE`, `IS NOT FALSE`, `IS UNKNOWN`, `IS NOT UNKNOWN`, `LIKE`, `NOT LIKE`, `ILIKE`, `NOT ILIKE` |
| Unary operators | unary `+`, unary `-` |
| Conditional expressions | `COALESCE()`, `NULLIF()` |
| Casts and expression forms | identity `CAST()`; text-family `CAST()` targets (`TEXT` / `NAME` / `VARCHAR`) from boolean / integer / floating / numeric / text-family inputs; `COLLATE`; `ARRAY[...]` literals |
| String functions | `UPPER()`, `LOWER()`, `LENGTH()`, `CHAR_LENGTH()`, `CHARACTER_LENGTH()`, `CONCAT()`, `CONCAT_WS()`, `LEFT()`, `RIGHT()`, `SUBSTRING()`, `SUBSTR()`, `TRIM()`, `BTRIM()`, `LTRIM()`, `RTRIM()`, `LPAD()`, `RPAD()`, `REPEAT()`, `REPLACE()`, `REVERSE()`, `INITCAP()`, `ASCII()`, `CHR()`, `STRPOS()`, `POSITION()`, `SPLIT_PART()`, `TRANSLATE()`, `QUOTE_IDENT()`, `QUOTE_LITERAL()`, `QUOTE_NULLABLE()`, `OVERLAY()`, `FORMAT()` |
| Mathematical functions | `ABS()` |
| Regular expression functions | `REGEXP_REPLACE()` |
| Regular expression operators | `~`, `~*`, `!~`, `!~*` |
| Array functions | `ARRAY_LENGTH()`, `ARRAY_UPPER()`, `ARRAY_LOWER()`, `CARDINALITY()`, `ARRAY_POSITION()`, `ARRAY_CAT()`, `ARRAY_APPEND()`, `ARRAY_PREPEND()`, `ARRAY_REMOVE()`, `ARRAY_TO_STRING()`, `STRING_TO_ARRAY()` |
| Encoding and hashing functions | `MD5()`, `SHA256()`, `DIGEST()`, `ENCODE()`, `DECODE()` |

## Pushdown control parameters

db9-server exposes one Boolean GUC for controlling DB9 Cop pushdown planning:

| Parameter | Scope | Default | Accepted values | Meaning |
| --- | --- | --- | --- | --- |
| `db9.enable_cop_pushdown` | session, or transaction-local via `SET LOCAL` | `off` | standard Boolean values such as `on` / `off`, `true` / `false`, `yes` / `no`, `1` / `0` | Master switch for DB9 Cop expression pushdown. Expressions outside the exact whitelist on this page still stay local even when it is enabled. |

Typical usage:

```sql
-- Enable normal DB9 Cop pushdown for the current session.
SET db9.enable_cop_pushdown = on;

-- Disable all DB9 Cop pushdown for the current session.
SET db9.enable_cop_pushdown = off;
```

Behavior notes:
- Aggregate functions currently stay local regardless of `db9.enable_cop_pushdown`.
- The parameter defaults to `off`.
- `EXPLAIN` is the recommended way to verify whether a query actually used DB9 Cop pushdown.

## Important restrictions

- Overloaded functions are pushed only for the signatures currently accepted by the planner. The pushdown checks in `src/sql/optimizer/pushdown/db9_cop.rs` remain authoritative.
- `CAST()` pushdown currently covers identity casts and text-family target casts only. Text-family target casts currently accept `BOOLEAN`, `INT`, `BIGINT`, `FLOAT`, `NUMERIC`, and existing text-family inputs.
- `POSITION(substr IN str)` is normalized to `STRPOS(str, substr)` before pushdown eligibility is checked.
- `TRIM(...)` is normalized to `BTRIM(...)`, `LTRIM(...)`, or `RTRIM(...)` before pushdown eligibility is checked.
- `LIKE` / `ILIKE` pushdown supports the standard optional `ESCAPE` clause. `SIMILAR TO` still stays local.
- `ABS()` is the only math builtin on the current exact whitelist. Other math builtins currently stay local.
- `REGEXP_REPLACE()` pushdown currently covers the 3-argument form, plus the 4-argument form only when the flags argument is a constant text string containing `g`, `i`, `x`, `p`, and/or `w`. Other flag families stay local.
- `REGEXP_SPLIT_TO_ARRAY()` currently stays local.
- `SUBSTRING()` / `SUBSTR()` pushdown currently covers positional forms only, for both text-family inputs and `BYTEA`. Regex-pattern overloads stay local.
- `OVERLAY()` pushdown currently covers same-family overloads only: text-family with text-family, or `BYTEA` with `BYTEA`. Mixed text/`BYTEA` overloads are not pushed.
- General `IN (...)` / `NOT IN (...)` pushdown is available for scalar equality-comparable types. Optimizer access-path rewrites such as `InListScan` remain a separate optimization.
- Array functions are pushed only for the signatures and element types currently accepted by the planner; the per-function type checks in `src/sql/optimizer/pushdown/db9_cop.rs` remain authoritative.

## Server-materialized and local-only cases

- `NOW()`, `CURRENT_TIMESTAMP`, `STATEMENT_TIMESTAMP()`, `TRANSACTION_TIMESTAMP()`, `CURRENT_DATE`, and `CURRENT_TIME` are materialized once per statement in `db9-server` and then participate in pushdown as constants.
- Temporal function calls such as `DATE()`, `DATE_PART()`, `EXTRACT()`, `DATE_TRUNC()`, `AGE()`, `TO_CHAR()`, `TO_TIMESTAMP()`, `MAKE_DATE()`, `MAKE_TIME()`, `MAKE_TIMESTAMP()`, and `MAKE_INTERVAL()` currently stay local on this exact pair.
- Unary bitwise `~`, bitwise operators `&`, `|`, `#`, `<<`, `>>`, and array operators `@>`, `<@`, `&&` currently stay local on this exact pair.
- JSON / JSONB functions and operators currently stay local on this exact pair, including `JSON_ARRAY_LENGTH()` / `JSONB_ARRAY_LENGTH()`, `JSON_TYPEOF()` / `JSONB_TYPEOF()`, `JSON_EXTRACT_PATH_TEXT()` / `JSONB_EXTRACT_PATH_TEXT()`, `JSONB_PRETTY()`, `JSONB_EXISTS()`, and the JSON access / containment / existence operator families.
- Aggregate functions currently stay local on this exact pair, including `COUNT()`, `SUM()`, `MIN()`, `MAX()`, `BOOL_AND()`, `BOOL_OR()`, `EVERY()`, aggregate `DISTINCT`, aggregate `FILTER`, aggregate `ORDER BY`, and aggregate-as-window forms.
- `STARTS_WITH()`, `TO_HEX()`, and `REGEXP_MATCH()` currently stay local.
- `RANDOM()`, `CLOCK_TIMESTAMP()`, `GEN_RANDOM_UUID()`, `UUID_GENERATE_V4()`, and `UUIDV7()` currently stay local.
- Set-returning functions and table functions such as `UNNEST()`, `JSON_ARRAY_ELEMENTS_TEXT()`, `REGEXP_SPLIT_TO_TABLE()`, and `REGEXP_MATCHES()` currently stay local.
