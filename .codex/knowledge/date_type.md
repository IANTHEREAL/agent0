# DATE type: implementation facts + code locations

## Design doc
- `docs/design/09_date_type.md`
  - Internal representation: `Value::Date(i32)` = days since `1970-01-01`.
  - pgwire OID: DATE = 1082.

## Types + helpers
- `src/types/mod.rs`
  - `DataType::Date` (appended; bincode compatibility).
  - `Value::Date(i32)` (appended).
  - `impl fmt::Display for Value` formats date via `crate::types::date::format_date_days(...)`.
- `src/types/date.rs`
  - `parse_date_days(s: &str) -> Result<i32>` parses `YYYY-MM-DD` into days since epoch.
  - `format_date_days(days: i32) -> Result<String>` formats days as `YYYY-MM-DD`.
  - `timestamp_millis_to_date_days(ts_millis: i64) -> Result<i32>` truncates UTC timestamp (ms) to date days.
  - `date_days_to_timestamp_millis(days: i32) -> Result<i64>` converts date days to UTC midnight timestamp (ms).
  - Unit tests: `parse_date_epoch_is_zero`, `date_round_trip`, `timestamp_truncates_to_utc_date`.

## Storage encoding (PK / secondary indexes)
- `src/storage/encoding.rs`
  - `encode_value_memcomparable(Value::Date(i32))` encodes as memcomparable i32.
  - `decode_value_memcomparable(..., DataType::Date)` decodes memcomparable i32 to `Value::Date(i32)`.

## SQL DDL/DML parsing + coercion
- `src/sql/helpers.rs`
  - `convert_data_type(SqlDataType::Date) -> DataType::Date` (DDL column type mapping).
  - `coerce_value_for_column(...)`:
    - `Value::Text("YYYY-MM-DD")` → `Value::Date(days)`.
    - `Value::Timestamp(ms)` → `Value::Date(days)` (UTC truncation via `timestamp_millis_to_date_days`).
  - `parse_value_for_copy(..., DataType::Date)` parses `YYYY-MM-DD` into `Value::Date`.
  - `value_to_sql_expr(Value::Date)` emits single-quoted `YYYY-MM-DD`.
  - `infer_expr_type(...)`:
    - `Expr::TypedString { data_type: Date, .. }` → `DataType::Date`.
    - `Expr::Interval(_)` → `DataType::Interval`.
    - `CURRENT_DATE` → `DataType::Date`.
    - `DATE(...)` → `DataType::Date`.
    - `Date +/- Interval` → `Timestamp`, `Date - Date` → `Int32`, `Timestamp +/- Interval` → `Timestamp`, `Timestamp - Timestamp` → `Interval`.

## Expression evaluation + comparison
- `src/sql/expr.rs`
  - `Expr::TypedString { data_type: Date, value }` evaluates via `parse_date_days(value)` → `Value::Date`.
  - `cast_value(..., Date)`:
    - `Text` → `Date` via `parse_date_days`.
    - `Timestamp` → `Date` via `timestamp_millis_to_date_days` (UTC truncation).
    - `Date` → `Timestamp` via `date_days_to_timestamp_millis` (UTC midnight).
  - `compare_values(...)` supports:
    - `Date` vs `Date`.
    - `Date` vs `Text` (parses RHS as date).
    - `Text` vs `Date` (parses LHS as date).
    - `Date` vs `Timestamp` (casts date to UTC midnight timestamp for comparison).
  - `CURRENT_DATE` returns `Value::Date(days)` computed from `Utc::now().date_naive()`.
  - `value_to_json(Value::Date)` encodes as JSON string `YYYY-MM-DD`.

## Date arithmetic (interval + subtraction)
- `src/sql/expr.rs`
  - `add_values(...)`:
    - `Date + Interval` and `Interval + Date` return `Timestamp` (date promoted to UTC midnight timestamp).
  - `sub_values(...)`:
    - `Date - Interval` returns `Timestamp` (date promoted to UTC midnight timestamp).
    - `Date - Date` returns `Int32` number of days.

## DATE() / AGE() functions
- `src/sql/expr.rs`
  - `DATE(...)`:
    - Returns `Value::Date` (both `eval_function_join` and `eval_function`).
    - Accepts `Date`, `Timestamp`, and `Text` (text parsed via `parse_timestamp_string`).
  - `AGE(...)`:
    - Accepts `Timestamp` and `Date` arguments (dates are converted to UTC midnight timestamp).

## pgwire
- `src/protocol/handler.rs`
  - `datatype_to_pgtype(Some(DataType::Date)) => Type::DATE` (OID 1082).
  - `encode_value(Value::Date(days))` outputs `YYYY-MM-DD` text.

## Introspection
- `src/sql/information_schema.rs`
  - `data_type_to_pg_type(DataType::Date) => "date"` (used by `information_schema.columns`).
  - `get_pg_attribute_rows(...)`:
    - `DataType::Date` → `atttypid = 1082`, `attlen = 4`.
  - `get_pg_type_rows(...)` includes a `pg_catalog.pg_type` row for `date` with `oid = 1082`.

## EXPLAIN formatting
- `src/sql/explain.rs`
  - `format_value(Value::Date)` prints `'YYYY-MM-DD'`.

## Integration tests
- `tests/44_date_type.sql`
  - DATE smoke coverage: DDL + inserts (`'YYYY-MM-DD'`, `DATE 'YYYY-MM-DD'`), ORDER BY, WHERE compare to text, `information_schema.columns`, JSON projection.
- `tests/45_date_type_improvements.sql`
  - Covers `CURRENT_DATE +/- INTERVAL`, `DATE - DATE`, date-vs-timestamp comparisons, `DATE(timestamp)`, and `AGE(date, date)`.
