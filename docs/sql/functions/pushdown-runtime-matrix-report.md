# Pushdown Runtime Matrix Report

## Status
- Rewritten on `2026-04-26` against the current review heads for this lane.
- Exact SHAs are intentionally omitted here because this report is a navigation aid and can go stale
  faster than the source-of-truth files below.
- This report is now only a **navigation aid**. The authoritative pushed surface remains:
  - `docs/sql/functions/list-of-expressions-for-pushdown.md`
  - `src/sql/optimizer/pushdown/db9_cop.rs`
  - `scripts/pg18_pushdown_oracle.py`
  - pushdown on/off regression gates
- Do **not** treat older copies of this report as release evidence; earlier revisions were stale and
  under-described the current admit surface.

## Exact Pair Read
- The current pair has no known **paired-contract drift** in the post-wave-1 increment set.
- However, broader pushdown on/off + PG18.3 review still requires the active runtime/oracle gates
  to stay green before merge.

## Bucket Matrix

| Bucket | Exact-pair verdict | Current pushed surface on the exact pair | Current local-only or constrained boundary | Exact evidence |
| --- | --- | --- | --- | --- |
| Operator | matched | Logical operators, comparison operators, null/boolean predicate forms, unary `+` / `-`, unary `~` (integer-only), bitwise `&` / `|` / `#` / `<<` / `>>` (integer-only), `LIKE` / `ILIKE`, `BETWEEN`, `IS DISTINCT FROM`, and the admitted `IN (...)` / `NOT IN (...)` subset | Array operators `@>`, `<@`, `&&` and regex predicate operators stay local | `src/sql/optimizer/pushdown/db9_cop.rs`, `tests/575_pushdown_operator_expressions.sql`, `tests/579_pushdown_operator_math_proof.sql` |
| Mathematical | matched | `ABS()`, `CEIL()`, `CEILING()`, `FLOOR()`, single-argument `ROUND()`, single-argument `TRUNC()`, `SQRT()`, `CBRT()`, `EXP()`, `LN()`, `LOG()`, `LOG10()`, `PI()`, `SIGN()`, `DEGREES()`, `RADIANS()`, `SIN()`, `COS()`, `TAN()`, `ASIN()`, `ACOS()`, `ATAN()`, `ATAN2()`, `MOD()`, `POWER()`, `POW()`, `WIDTH_BUCKET()`, `HASHTEXT()` | Type-gated exactly as implemented by `db9_cop.rs`; `TRUNCATE()`, precision `ROUND()` / `TRUNC()`, and multi-arg `LOG(...)` stay local/unsupported | `src/sql/optimizer/pushdown/db9_cop.rs`, `src/coprocessor/db9/functions/math.rs`, `tests/579_pushdown_operator_math_proof.sql` |
| Date/time | matched | `DATE()` (`DATE` / `TIMESTAMP` input subset), `DATE_PART()` / `EXTRACT()` (timestamp-safe subset), `DATE_TRUNC()` (timestamp-safe subset), `TO_CHAR()` (bounded timestamp-safe format subset), two-argument non-`TIMESTAMPTZ` `AGE()`, `MAKE_DATE()`, `MAKE_TIME()` | timestamptz-sensitive cases, `DATE(TEXT)`, interval `DATE_PART()` / `EXTRACT()`, unsupported or oversized format cases, `MAKE_TIMESTAMP()`, `MAKE_INTERVAL()`, and numeric `TO_TIMESTAMP()` stay local | `src/sql/optimizer/pushdown/db9_cop.rs`, `src/coprocessor/db9/functions/datetime.rs`, `tests/566_pushdown_datetime_types.sql`, `tests/573_pushdown_interval_scalars.sql` |
| String / regex | matched | Admitted text helpers and `substring/substr` positional forms | `LOWER()`, `UPPER()`, `CONCAT()`, `CONCAT_WS()`, `LPAD()`, `RPAD()`, `REPEAT()`, `REPLACE()`, `INITCAP()`, `TRANSLATE()`, quote helpers, `OVERLAY()`, `FORMAT()`, regex predicate operators, `REGEXP_REPLACE()`, `REGEXP_SPLIT_TO_ARRAY()`, and scalar `REGEXP_MATCH()` stay local on this exact pair; set-returning `REGEXP_MATCHES()` and `REGEXP_SPLIT_TO_TABLE()` also stay local; bytea/text edge semantics must continue to be guarded by PG18 oracle + on/off gate | `src/sql/optimizer/pushdown/db9_cop.rs`, `src/sql/expr/functions/string.rs`, `src/coprocessor/db9/functions/string.rs`, `scripts/pg18_pushdown_oracle.py` |
| JSON / JSONB | local-only | None | JSON / JSONB scalar functions, access operators, containment operators, and existence operators all stay local on this pair | `tests/574_pushdown_json_scalars.sql`, `tests/574_pushdown_json_scalars.assert` |
| Array | matched | `array_length`, `array_upper`, `array_lower`, `cardinality`, `array_position`, `array_cat`, `array_append`, `array_prepend`, `array_remove`, `string_to_array` | `array_to_string` and array operators `@>`, `<@`, `&&` stay local | `tests/576_pushdown_array_operators.sql`, `tests/578_pushdown_array_function_parity.sql` |
| Aggregate / statistical | local-only | None | Public contract still keeps aggregates local on this pair | `tests/571_pushdown_partial_aggregate_grouped.assert`, `tests/580_pushdown_rejected_agg_guc.expected` |

## Current Evidence Map
- `docs/sql/functions/list-of-expressions-for-pushdown.md`
  - current planner/document contract baseline
- `src/sql/optimizer/pushdown/db9_cop.rs`
  - authoritative planner admit surface
- `src/sql/expr/functions/string.rs`
  - db9 local string/bytea fallback semantics
- `src/coprocessor/db9/functions/string.rs`
  - paired runtime string/bytea semantics
- `scripts/pg18_pushdown_oracle.py`
  - PG18.3 oracle and on/off parity cases
- `tests/571_pushdown_partial_aggregate_grouped.assert`
  - grouped aggregate local-only contract
- `tests/580_pushdown_rejected_agg_guc.expected`
  - stale public aggregate-only GUC rejection contract

## Closeout Read
- The current exact pair is content-complete for this report rewrite.
- `#13` and `#15` may still exist as separate docs-only task-board lanes until their ownership and
  disposition are explicitly absorbed or closed out, but that audit state does not change the exact
  bucket conclusions above.
- If `docs/sql/functions/list-of-expressions-for-pushdown.md` later lands the pending precision
  edits from `#13` and `#15`, it should align to this report rather than force any bucket
  reclassification.
