# Code Review: NUMERIC/DECIMAL Type Implementation

This change adds `NUMERIC`/`DECIMAL` support using `rust_decimal`. It touches: type system, expression evaluation (literals/arithmetic/casts), aggregation, storage/index encoding, and pgwire/COPY output.

## What Looks Solid

- `DataType::Numeric { precision, scale }` + `Value::Numeric(Decimal)` integration is consistent across executor → protocol.
- `information_schema.columns` reports `numeric_precision`/`numeric_scale`.
- Precision/scale constraints are now aligned with `rust_decimal` (max 28 digits / scale 0–28) and validated in DDL/CAST paths.

## Fixed Issues

### 1. Index/PK ordering for NUMERIC (High) — FIXED

**Location:** `src/storage/encoding.rs`

The original memcomparable numeric encoding was not order-preserving for some fractional values (e.g. `0.1` vs `0.09`) and couldn’t reliably decode values whose mantissa ends in `0` (e.g. `100`) because decoding inferred digit-length by trimming trailing zeros.

**Fix:** switched to a canonical, order-preserving fixed-length encoding:

- format: `[sign][biased_exp][digits[28]][digits_len]`
- canonicalization via `Decimal::normalize_assign()`
- negative values invert the exponent+digits bytes
- `digits_len` preserves mantissa length for decoding

Added tests for ordering + canonicalization + decoding round-trip.

### 2. NUMERIC treated as FLOAT8 in parsing/casts/type inference (High) — FIXED

**Locations:** `src/sql/expr.rs`, `src/sql/helpers.rs`

Previously:
- `CAST(... AS NUMERIC)` produced `Float64`
- `SqlDataType::Numeric/Decimal` mapped to `DataType::Float64` in some inference paths
- decimal literals (`3.14`) evaluated to `Float64`

Now:
- decimal literals without exponent parse as `Value::Numeric`
- `CAST(... AS NUMERIC/DECIMAL)` produces `Value::Numeric` and enforces `p/s` constraints
- type inference/DDL conversion consistently returns `DataType::Numeric`

### 3. Silent “convert to 0.0” fallbacks (Medium) — FIXED

**Locations:** `src/sql/aggregate.rs`, `src/sql/expr.rs`

Places that previously did `unwrap_or(0.0)` on Decimal→f64 conversion now return an error instead, preventing silent wrong results.

## Remaining Considerations

- Precision enforcement after arithmetic: results can exceed declared `precision` at runtime; enforcement is currently only at DDL/CAST/coercion points. This is probably acceptable for MVP, but it’s a semantic choice.
- Literal semantics: exponent-form literals (e.g. `1e-3`) still evaluate as `Float64` (not `Numeric`). If ORM/Postgres compatibility demands “exact numeric” constants for exponent form too, this may need revisiting.
