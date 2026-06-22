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
| Comparison operators | `=`, `<>`, `<`, `<=`, `>`, `>=`, `IS NULL`, `IS NOT NULL`, `IS DISTINCT FROM`, `IS NOT DISTINCT FROM` (subject to the exact scalar / array type-family restrictions listed below) |
| Predicate operators | `BETWEEN`, `NOT BETWEEN`, `IN (...)`, `NOT IN (...)`, `IS TRUE`, `IS NOT TRUE`, `IS FALSE`, `IS NOT FALSE`, `IS UNKNOWN`, `IS NOT UNKNOWN`, `LIKE`, `NOT LIKE`, `ILIKE`, `NOT ILIKE` (subject to the exact equality-family restrictions listed below) |
| Unary operators | unary `+`, unary `-`, unary `~` (integer-only subset) |
| Bitwise operators | `&`, `|`, `#`, `<<`, `>>` (integer-only subset) |
| Conditional expressions | `COALESCE()`, `NULLIF()` |
| Expression forms | `ARRAY[...]` literals (wire-safe constant subset) |
| String functions | `LENGTH()`, `CHAR_LENGTH()`, `CHARACTER_LENGTH()`, `LEFT()`, `RIGHT()`, `SUBSTRING()`, `SUBSTR()`, `BTRIM()`, `LTRIM()`, `RTRIM()`, `REVERSE()`, `ASCII()`, `CHR()`, `STRPOS()`, `POSITION()`, `SPLIT_PART()` |
| Mathematical functions | `ABS()`, `CEIL()`, `CEILING()`, `FLOOR()`, `ROUND()`, `TRUNC()`, `SQRT()`, `CBRT()`, `EXP()`, `LN()`, `LOG()`, `LOG10()`, `PI()`, `SIGN()`, `DEGREES()`, `RADIANS()`, `SIN()`, `COS()`, `TAN()`, `ASIN()`, `ACOS()`, `ATAN()`, `ATAN2()`, `MOD()`, `POWER()`, `POW()`, `WIDTH_BUCKET()`, `HASHTEXT()` |
| Temporal functions | `DATE_PART()` / `EXTRACT()` (timestamp safe-field subset), `DATE_TRUNC()` (timestamp safe-field subset), `TO_CHAR()` (bounded timestamp safe-format subset), `DATE()` (`DATE` / `TIMESTAMP` input subset), `AGE()` (non-`TIMESTAMPTZ` two-argument subset), `MAKE_DATE()`, `MAKE_TIME()` |
| Regular expression operators and functions | Operators (`~`, `!~`, `~*`, `!~*`) and scalar regex functions currently stay local on this exact pair. |
| Array functions | `ARRAY_LENGTH()`, `ARRAY_UPPER()`, `ARRAY_LOWER()`, `CARDINALITY()`, `ARRAY_POSITION()`, `ARRAY_CAT()`, `ARRAY_APPEND()`, `ARRAY_PREPEND()`, `ARRAY_REMOVE()`, `STRING_TO_ARRAY()` |
| Encoding and hashing functions | `MD5()`, `SHA256()`, `DIGEST()`, `DECODE()` |

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
- Enable this parameter only after every paired `cloud-storage-engine` / TiKV node has been upgraded to the runtime surface that matches this `db9-server` build. DB9 Cop requests carry an exact `codec_version` surface gate, so mismatched runtime pairs fail fast instead of falling back locally; keep pushdown disabled during rolling upgrades unless every node already runs the paired runtime. DB9 Cop requests still do not carry per-function capability negotiation and runtime "unsupported DB9 ..." errors are not retried locally.
- DB9 Cop pushdown can use table scans and covered secondary B-tree index scans (`point`, prefix/range, bounded range, and IN-list) on this exact pair. Secondary-index DB9 Cop scans are index-only: every pushed filter/projection/output column must be available from index key columns or the PK carried by the index entry, every covered index/PK column must use the scalar index-only type whitelist in `src/sql/optimizer/pushdown/db9_cop.rs`, and non-covered secondary-index plans stay local instead of requesting storage-side row fetch. Covered ordered prefix/range scans can avoid a local sort for uniform-direction `ORDER BY ... LIMIT/OFFSET` shapes when the remaining index key columns, plus the non-unique PK suffix, satisfy the requested order.
- `db9.enable_cop_agg_pushdown` is not a supported public setting on this exact pair. `SHOW`, `current_setting(...)`, `SET`, and `RESET` against that stale name fail with SQLSTATE `42704`; this is an intentional compatibility break from earlier silent acceptance of unknown dotted names, to keep the public DB9 Cop contract one-switch only.
- Boolean `AND` / `OR` evaluation preserves PostgreSQL-style planner constant-folding behavior before runtime short-circuiting. Foldable constant subtrees may raise errors even when a row-dependent sibling would otherwise make the Boolean result obvious, and the local evaluator and DB9 Cop evaluator intentionally share that behavior.
- DB9 Cop predicate evaluation is request-scoped and does not shrink with the current response batch size. This avoids response-size-dependent failures when a pushed filter builds intermediate values. On this exact pair, `cloud-storage-engine` clamps both stored-row decode budget and predicate/selection intermediate string output budget to `[32 MiB, 64 MiB]` derived from TiKV `cop_max_resp_size`, so a very small `cop_max_resp_size` does not act as a hard execution memory cap.
- `EXPLAIN` is the recommended way to verify whether a query actually used DB9 Cop pushdown.

## Important restrictions

- Overloaded functions are pushed only for the signatures currently accepted by the planner. The pushdown checks in `src/sql/optimizer/pushdown/db9_cop.rs` remain authoritative.
- Comparison pushdown is narrower than the full scalar type system on this exact pair. Scalar `=` / `<>` / distinctness checks currently push only for boolean, primitive numeric, text-family, `BYTEA`, `TIMESTAMP`, and `TIMESTAMPTZ`, while scalar ordering currently pushes only for primitive numeric, text-family, `TIMESTAMP`, and `TIMESTAMPTZ`. Matching array equality pushdown follows the same element-family gate. Scalar `DATE` / `TIME` / `UUID` comparison still stays local on this exact pair. `IN (...)` / `NOT IN (...)` pushdown is available only when the left expression and every list item belong to the supported scalar equality families above.
- `ARRAY[...]` literal pushdown is limited by the current DB9 wire-safe constant carrier. Constant-literal arrays containing `DATE`, `TIME`, `INTERVAL`, `UUID`, `JSON`, `JSONB`, or `NUMERIC` elements currently stay local, even when surrounding array operators or array helper functions are otherwise admitted.
- DB9 Cop output schema support is intentionally narrower than the full DB9 SQL type system on this exact pair. If any projected column type is outside the exact supported output set, pushdown stays local even when every expression is otherwise eligible.
- The newly opened math pushdown surface is still signature-gated. In particular, `CEIL` / `FLOOR` / single-argument `ROUND` / single-argument `TRUNC` currently push only for the primitive numeric forms accepted by the exact-pair planner, while broader PG-compatible numeric/date-time variants still stay local until later paired waves open them. PostgreSQL-compatible `TRUNCATE()` is not part of this exact-pair surface; use `TRUNC()` instead.
- The domain-checked math tier also remains deliberately narrow on this exact pair: `SQRT`, `EXP`, `LN`, `LOG`, `LOG10`, `ASIN`, and `ACOS` currently push only in the unary forms accepted by `src/sql/optimizer/pushdown/db9_cop.rs`. Multi-argument `LOG(...)` variants still stay local until a later paired wave opens them explicitly.
- The newly opened temporal surface is also deliberately signature- and field-gated on this exact pair. `DATE_PART` / `EXTRACT` currently push only for timestamp sources with the exact safe field set already admitted by `src/sql/optimizer/pushdown/db9_cop.rs`; interval-only and timestamptz-sensitive cases outside that set still stay local. `DATE_TRUNC` currently pushes only for timestamp sources with `year` / `month` / `day` / `hour` / `minute` / `second`, and `TO_CHAR` currently pushes only for timestamp sources with a bounded constant format string composed from the exact token subset already implemented by the paired runtime.
- The newly opened bitwise surface is integer-only. `&`, `|`, `#`, `<<`, `>>`, and unary `~` currently push only for `int4` / `int8` combinations already accepted by `src/sql/optimizer/pushdown/db9_cop.rs`. Shift counts follow PostgreSQL's masked-count semantics for the underlying integer width.
- `POSITION(substr IN str)` is normalized to `STRPOS(str, substr)` before pushdown eligibility is checked.
- `TRIM(...)` is normalized to `BTRIM(...)`, `LTRIM(...)`, or `RTRIM(...)` before pushdown eligibility is checked.
- `LIKE` / `ILIKE` pushdown supports the standard optional `ESCAPE` clause. `SIMILAR TO` still stays local.
- `SUBSTRING()` / `SUBSTR()` pushdown currently covers positional forms only, for both text-family inputs and `BYTEA`. Regex-pattern overloads stay local.
- `OVERLAY()` currently stays local on this exact pair, including same-family text and `BYTEA` overloads.
- General `IN (...)` / `NOT IN (...)` pushdown is available for scalar equality-comparable types. Optimizer access-path rewrites such as `InListScan` remain a separate optimization.
- Array functions are pushed only for the signatures and element types currently accepted by the planner; the per-function type checks in `src/sql/optimizer/pushdown/db9_cop.rs` remain authoritative.

## Server-materialized and local-only cases

- `NOW()`, `CURRENT_TIMESTAMP`, `STATEMENT_TIMESTAMP()`, `TRANSACTION_TIMESTAMP()`, `CURRENT_DATE`, and `CURRENT_TIME` are materialized once per statement in `db9-server` and then participate in pushdown as constants.
- `CAST()` currently stays local on this exact pair.
- String helpers outside the pushed subset stay local on this exact pair, including `UPPER()`, `LOWER()`, `CONCAT()`, `CONCAT_WS()`, `LPAD()`, `RPAD()`, `REPEAT()`, `REPLACE()`, `INITCAP()`, `TRANSLATE()`, `QUOTE_IDENT()`, `QUOTE_LITERAL()`, `QUOTE_NULLABLE()`, `OVERLAY()`, and `FORMAT()`.
- Temporal functions still stay local when they hit the exact-head unsupported surface, including timestamptz-sensitive overloads of `DATE_PART` / `EXTRACT` / `DATE_TRUNC`, interval overloads of `DATE_PART` / `EXTRACT`, `DATE(TIMESTAMPTZ)`, `DATE(TEXT)`, `AGE()` with any `TIMESTAMPTZ` argument, unsupported `DATE_TRUNC` fields, unsupported or oversized `TO_CHAR` format patterns, and `MAKE_TIMESTAMP()` / `MAKE_INTERVAL()` / `TO_TIMESTAMP()` on this exact pair. The current planner already admits a narrower pushed subset including timestamp-safe `DATE_PART` / `EXTRACT`, timestamp-safe `DATE_TRUNC`, timestamp-safe `TO_CHAR`, `DATE(DATE)` / `DATE(TIMESTAMP)`, two-argument non-`TIMESTAMPTZ` `AGE()`, `MAKE_DATE()`, and `MAKE_TIME()`.
- Array operators `@>`, `<@`, `&&` currently stay local on this exact pair.
- `ARRAY_TO_STRING()` currently stays local on this exact pair.
- JSON / JSONB functions and operators currently stay local on this exact pair, including `JSON_ARRAY_LENGTH()` / `JSONB_ARRAY_LENGTH()`, `JSON_TYPEOF()` / `JSONB_TYPEOF()`, `JSON_EXTRACT_PATH_TEXT()` / `JSONB_EXTRACT_PATH_TEXT()`, `JSONB_PRETTY()`, `JSONB_EXISTS()`, and the JSON access / containment / existence operator families.
- Aggregate functions currently stay local on this exact pair, including `COUNT()`, `SUM()`, `MIN()`, `MAX()`, `BOOL_AND()`, `BOOL_OR()`, `EVERY()`, aggregate `DISTINCT`, aggregate `FILTER`, aggregate `ORDER BY`, and aggregate-as-window forms.
- Regex operators `~`, `!~`, `~*`, and `!~*` currently stay local on this exact pair.
- `REGEXP_REPLACE()`, `REGEXP_SPLIT_TO_ARRAY()`, and `REGEXP_MATCH()` currently stay local on this exact pair.
- `ENCODE()` currently stays local on this exact pair.
- `STARTS_WITH()` and `TO_HEX()` currently stay local.
- `RANDOM()`, `CLOCK_TIMESTAMP()`, `GEN_RANDOM_UUID()`, `UUID_GENERATE_V4()`, and `UUIDV7()` currently stay local.
- Set-returning functions and table functions such as `UNNEST()`, `JSON_ARRAY_ELEMENTS_TEXT()`, `REGEXP_SPLIT_TO_TABLE()`, and `REGEXP_MATCHES()` currently stay local.
