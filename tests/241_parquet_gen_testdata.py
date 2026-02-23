#!/usr/bin/env python3
"""Generate test Parquet files for pg-tikv parquet extension integration tests.

Requires: pip install pyarrow

Usage:
  python3 tests/241_parquet_gen_testdata.py [output_dir]

Generates:
  - basic.parquet         : 100 rows, id(int32) + name(utf8) + value(float64)
  - types.parquet         : 10 rows testing multiple data types
  - empty.parquet         : 0 rows, schema only
  - large_groups.parquet  : 10,000 rows across multiple row groups
  - nulls.parquet         : rows with various null patterns
"""

import os
import sys

def main():
    try:
        import pyarrow as pa
        import pyarrow.parquet as pq
    except ImportError:
        print("SKIP: pyarrow not installed (pip install pyarrow)", file=sys.stderr)
        sys.exit(0)

    out_dir = sys.argv[1] if len(sys.argv) > 1 else "tests/parquet_testdata"
    os.makedirs(out_dir, exist_ok=True)

    # --- basic.parquet: simple 3-column table ---
    table = pa.table({
        "id": pa.array(range(100), type=pa.int32()),
        "name": pa.array([f"row_{i}" for i in range(100)], type=pa.utf8()),
        "value": pa.array([float(i) * 1.5 for i in range(100)], type=pa.float64()),
    })
    pq.write_table(table, os.path.join(out_dir, "basic.parquet"))
    print(f"wrote {out_dir}/basic.parquet (100 rows)")

    # --- types.parquet: multiple data types ---
    table = pa.table({
        "bool_col": pa.array([True, False] * 5, type=pa.bool_()),
        "int32_col": pa.array(range(10), type=pa.int32()),
        "int64_col": pa.array(range(10), type=pa.int64()),
        "float32_col": pa.array([float(i) for i in range(10)], type=pa.float32()),
        "float64_col": pa.array([float(i) * 0.1 for i in range(10)], type=pa.float64()),
        "text_col": pa.array([f"text_{i}" for i in range(10)], type=pa.utf8()),
    })
    pq.write_table(table, os.path.join(out_dir, "types.parquet"))
    print(f"wrote {out_dir}/types.parquet (10 rows)")

    # --- empty.parquet: schema only, no rows ---
    schema = pa.schema([
        pa.field("id", pa.int32()),
        pa.field("name", pa.utf8()),
    ])
    table = pa.table({"id": pa.array([], type=pa.int32()), "name": pa.array([], type=pa.utf8())})
    pq.write_table(table, os.path.join(out_dir, "empty.parquet"))
    print(f"wrote {out_dir}/empty.parquet (0 rows)")

    # --- large_groups.parquet: multiple row groups ---
    table = pa.table({
        "id": pa.array(range(10000), type=pa.int32()),
        "category": pa.array([f"cat_{i % 10}" for i in range(10000)], type=pa.utf8()),
        "amount": pa.array([float(i % 1000) for i in range(10000)], type=pa.float64()),
    })
    pq.write_table(table, os.path.join(out_dir, "large_groups.parquet"),
                    row_group_size=1000)
    print(f"wrote {out_dir}/large_groups.parquet (10000 rows, 10 row groups)")

    # --- nulls.parquet: null patterns ---
    table = pa.table({
        "id": pa.array(range(10), type=pa.int32()),
        "nullable_text": pa.array(
            [None if i % 3 == 0 else f"val_{i}" for i in range(10)],
            type=pa.utf8()
        ),
        "nullable_int": pa.array(
            [None if i % 2 == 0 else i for i in range(10)],
            type=pa.int32()
        ),
    })
    pq.write_table(table, os.path.join(out_dir, "nulls.parquet"))
    print(f"wrote {out_dir}/nulls.parquet (10 rows with nulls)")

    print(f"\nAll test parquet files written to {out_dir}/")

if __name__ == "__main__":
    main()
