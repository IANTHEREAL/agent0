# Bug: Incorrect Type OID Returned for Numeric Results

## Summary

pg-tikv returns incorrect PostgreSQL type OIDs for numeric query results, causing client drivers (e.g., pg8000) to fail when parsing values.

## Reproduction

1. Execute `examples/fancy_demo.sql` via a PostgreSQL client using pg8000 driver
2. Queries involving `SUM()`, `AVG()`, or other aggregate functions on `DOUBLE PRECISION` columns fail

## Error

```
invalid literal for int() with base 10: '4209.95'
```

## Root Cause

pg-tikv returns integer type OIDs (e.g., OID 20 for `int8/bigint`) for values that are actually floating-point numbers (e.g., `4209.95`).

When pg8000 receives the result:
1. It reads the type OID from the wire protocol (e.g., OID 20 = bigint)
2. It uses `int()` to convert the string value
3. The value `'4209.95'` cannot be parsed as an integer → exception

## Expected Behavior

pg-tikv should return the correct type OID based on the actual result type:
- `DOUBLE PRECISION` results → OID 701 (`float8`)
- `REAL` results → OID 700 (`float4`)
- `NUMERIC` results → OID 1700 (`numeric`)
- `INTEGER` results → OID 23 (`int4`)
- `BIGINT` results → OID 20 (`int8`)

## Affected Code

Likely in `src/protocol/handler.rs` → `datatype_to_pgtype()` or the result encoding logic.

The issue may occur when:
1. Aggregate functions (`SUM`, `AVG`, etc.) don't properly infer the result type
2. Type casting (`::numeric`, `::float`) doesn't update the OID
3. Expression evaluation returns a different type than declared

## Workaround

cloud-admin-portal uses a temporary workaround in `backend/app/services/pg_client.py`:

```python
def _tolerant_int(val: str) -> int:
    try:
        return int(val)
    except ValueError:
        return int(float(val))

pg8000.converters.PG_TYPES[20] = _tolerant_int  # int8
pg8000.converters.PG_TYPES[21] = _tolerant_int  # int2
pg8000.converters.PG_TYPES[23] = _tolerant_int  # int4
```

This is not a proper fix - it truncates decimals and masks the underlying protocol issue.

## PostgreSQL Type OID Reference

| OID | Type Name | Python Converter |
|-----|-----------|------------------|
| 20 | int8/bigint | `int` |
| 21 | int2/smallint | `int` |
| 23 | int4/integer | `int` |
| 700 | float4/real | `float` |
| 701 | float8/double precision | `float` |
| 1700 | numeric/decimal | `Decimal` |

## Test Case

```sql
-- This should return OID 701 (float8), not OID 20 (int8)
SELECT SUM(total) FROM orders WHERE status != 'cancelled';

-- Result: 4209.95
-- Expected OID: 701
-- Actual OID: 20 (causes parsing failure)
```
