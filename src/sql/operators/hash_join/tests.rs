//! Tests for the hash join module.

use super::*;
use crate::model::{ColumnDef, DataType};

fn schema_left() -> TableSchema {
    TableSchema {
        name: "left".to_string(),
        table_id: 1,
        columns: vec![
            ColumnDef {
                name: "id".to_string(),
                data_type: DataType::Int32,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                generation_expr: None,
                generation_expr_authorized_by: None,
                collation: None,
            },
            ColumnDef {
                name: "l".to_string(),
                data_type: DataType::Text,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                generation_expr: None,
                generation_expr_authorized_by: None,
                collation: None,
            },
        ],
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        rls_enabled: false,
        rls_force: false,
        from_alias: None,
    }
}

fn schema_right() -> TableSchema {
    TableSchema {
        name: "right".to_string(),
        table_id: 2,
        columns: vec![
            ColumnDef {
                name: "id".to_string(),
                data_type: DataType::Int32,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                generation_expr: None,
                generation_expr_authorized_by: None,
                collation: None,
            },
            ColumnDef {
                name: "r".to_string(),
                data_type: DataType::Text,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                generation_expr: None,
                generation_expr_authorized_by: None,
                collation: None,
            },
        ],
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        rls_enabled: false,
        rls_force: false,
        from_alias: None,
    }
}

#[test]
fn test_hash_int32_int64_compatible() {
    assert_eq!(
        hash_join_key(&[Value::Int32(42)]),
        hash_join_key(&[Value::Int64(42)])
    );
    assert!(join_keys_equal(&[Value::Int32(42)], &[Value::Int64(42)]));
}

#[test]
fn test_numeric_normalization_hash_equal() {
    use rust_decimal::Decimal;
    use std::str::FromStr;

    let d1 = Decimal::from_str("1.0").unwrap();
    let d2 = Decimal::from_str("1.00").unwrap();
    assert_eq!(
        hash_join_key(&[Value::Numeric(d1)]),
        hash_join_key(&[Value::Numeric(d2)])
    );
    assert!(join_keys_equal(
        &[Value::Numeric(d1)],
        &[Value::Numeric(d2)]
    ));
}

#[test]
fn test_float64_nan_hash_equal_and_join_equal() {
    // Use different NaN bit patterns to ensure the join treats all NaNs as equal.
    let nan1 = f64::from_bits(0x7ff8_0000_0000_0001);
    let nan2 = f64::from_bits(0xfff8_0000_0000_0002);
    assert!(nan1.is_nan());
    assert!(nan2.is_nan());

    assert!(join_keys_equal(
        &[Value::Float64(nan1)],
        &[Value::Float64(nan2)]
    ));
    assert_eq!(
        hash_join_key(&[Value::Float64(nan1)]),
        hash_join_key(&[Value::Float64(nan2)])
    );
}

#[test]
fn test_float64_negative_zero_hash_equal_and_join_equal() {
    assert!(join_keys_equal(
        &[Value::Float64(-0.0)],
        &[Value::Float64(0.0)]
    ));
    assert_eq!(
        hash_join_key(&[Value::Float64(-0.0)]),
        hash_join_key(&[Value::Float64(0.0)])
    );
}

#[test]
fn test_hash_table_insert_and_probe() {
    let mut table = JoinHashTable::new(vec![0]);
    table.insert(Row::new(vec![Value::Int32(1), Value::Text("a".into())]));
    table.insert(Row::new(vec![Value::Int32(1), Value::Text("b".into())]));
    table.insert(Row::new(vec![Value::Int32(2), Value::Text("c".into())]));
    table.finalize();

    // Probe by values using the table's key indices.
    let hash = hash_join_key(&[Value::Int32(1)]);
    let bucket = table.buckets.get(&hash).unwrap();
    assert_eq!(bucket.rows.len(), 2);

    assert!(table.row_key_equals_values(&bucket.rows[0], &[Value::Int32(1)]));
    assert!(table.row_key_equals_values(&bucket.rows[1], &[Value::Int32(1)]));
    assert!(!table.row_key_equals_values(&bucket.rows[0], &[Value::Int32(2)]));
}

#[test]
fn test_hash_join_outer_side_mapping_is_logical() {
    // Validate the critical mapping: join type is relative to logical left/right,
    // while build/probe are chosen independently (left_is_build).
    let left_child: BoxedOperator =
        Box::new(super::super::scan::TableScanOperator::new(schema_left()));
    let right_child: BoxedOperator =
        Box::new(super::super::scan::TableScanOperator::new(schema_right()));

    let op = HashJoinOperator::new(
        left_child,
        right_child,
        HashJoinType::Left,
        vec![0],
        vec![0],
        true, // left is build
        None,
        HashJoinConfig::default(),
    );
    assert!(op.build_outer);
    assert!(!op.probe_outer);

    let left_child: BoxedOperator =
        Box::new(super::super::scan::TableScanOperator::new(schema_left()));
    let right_child: BoxedOperator =
        Box::new(super::super::scan::TableScanOperator::new(schema_right()));
    let op = HashJoinOperator::new(
        left_child,
        right_child,
        HashJoinType::Left,
        vec![0],
        vec![0],
        false, // right is build (left is probe)
        None,
        HashJoinConfig::default(),
    );
    assert!(!op.build_outer);
    assert!(op.probe_outer);
}

#[test]
fn test_hash_table_empty_probe() {
    let table = JoinHashTable::new(vec![0]);

    let hash = hash_join_key(&[Value::Int32(1)]);
    assert!(!table.buckets.contains_key(&hash));
}

#[test]
fn test_hash_table_many_duplicate_keys() {
    let mut table = JoinHashTable::new(vec![0]);
    for i in 0..100 {
        table.insert(Row::new(vec![
            Value::Int32(1),
            Value::Text(format!("row_{}", i)),
        ]));
    }
    table.finalize();

    let hash = hash_join_key(&[Value::Int32(1)]);
    let bucket = table.buckets.get(&hash).unwrap();
    assert_eq!(bucket.rows.len(), 100);

    for row in &bucket.rows {
        assert!(table.row_key_equals_values(row, &[Value::Int32(1)]));
    }
}

#[test]
fn test_hash_join_full_outer_mapping() {
    let left_child: BoxedOperator =
        Box::new(super::super::scan::TableScanOperator::new(schema_left()));
    let right_child: BoxedOperator =
        Box::new(super::super::scan::TableScanOperator::new(schema_right()));

    let op = HashJoinOperator::new(
        left_child,
        right_child,
        HashJoinType::Full,
        vec![0],
        vec![0],
        true,
        None,
        HashJoinConfig::default(),
    );

    assert!(op.build_outer);
    assert!(op.probe_outer);
}

#[test]
fn test_hash_join_config_default_value() {
    let cfg = HashJoinConfig::default();
    assert_eq!(cfg.max_memory_bytes, 256 * 1024 * 1024);
}

#[test]
fn test_hash_join_constructor_swaps_build_probe_keys_when_right_is_build() {
    let left_child: BoxedOperator =
        Box::new(super::super::scan::TableScanOperator::new(schema_left()));
    let right_child: BoxedOperator =
        Box::new(super::super::scan::TableScanOperator::new(schema_right()));

    let op = HashJoinOperator::new(
        left_child,
        right_child,
        HashJoinType::Inner,
        vec![0], // left key
        vec![1], // right key
        false,   // right as build
        None,
        HashJoinConfig::default(),
    );

    assert!(!op.left_is_build);
    assert_eq!(op.build_key_indices, vec![1]);
    assert_eq!(op.probe_key_indices, vec![0]);
    assert!(!op.build_outer);
    assert!(!op.probe_outer);
}

#[test]
fn test_hash_join_right_join_outer_mapping_with_left_build() {
    let left_child: BoxedOperator =
        Box::new(super::super::scan::TableScanOperator::new(schema_left()));
    let right_child: BoxedOperator =
        Box::new(super::super::scan::TableScanOperator::new(schema_right()));

    let op = HashJoinOperator::new(
        left_child,
        right_child,
        HashJoinType::Right,
        vec![0],
        vec![0],
        true, // left is build
        None,
        HashJoinConfig::default(),
    );

    assert!(!op.build_outer);
    assert!(op.probe_outer);
}

#[test]
fn test_hash_join_output_schema_keeps_left_then_right_column_order() {
    let left_child: BoxedOperator =
        Box::new(super::super::scan::TableScanOperator::new(schema_left()));
    let right_child: BoxedOperator =
        Box::new(super::super::scan::TableScanOperator::new(schema_right()));

    let op = HashJoinOperator::new(
        left_child,
        right_child,
        HashJoinType::Inner,
        vec![0],
        vec![0],
        true,
        None,
        HashJoinConfig::default(),
    );

    let names: Vec<String> = op.schema().columns.iter().map(|c| c.name.clone()).collect();
    assert_eq!(
        names,
        vec![
            "id".to_string(),
            "l".to_string(),
            "id".to_string(),
            "r".to_string()
        ]
    );
}

#[test]
fn test_join_key_helpers_handle_length_and_missing_indices() {
    assert!(!join_keys_equal(&[Value::Int32(1)], &[]));

    let row = Row::new(vec![Value::Int32(1)]);
    assert!(row_key_has_null_for_join(&row, &[1])); // out-of-bounds treated as NULL
    assert!(!row_key_has_null_for_join(&row, &[0]));

    let row_missing = Row::new(vec![]);
    let row_null = Row::new(vec![Value::Null]);
    assert_eq!(
        hash_row_key_for_join(&row_missing, &[0]),
        hash_row_key_for_join(&row_null, &[0])
    );
}

#[test]
fn test_row_keys_equal_for_join_cross_indices_and_length_mismatch() {
    let build_row = Row::new(vec![Value::Int32(7), Value::Text("x".into())]);
    let probe_row = Row::new(vec![Value::Text("y".into()), Value::Int64(7)]);
    assert!(row_keys_equal_for_join(&build_row, &[0], &probe_row, &[1]));
    assert!(!row_keys_equal_for_join(
        &build_row,
        &[0, 1],
        &probe_row,
        &[1]
    ));
}

#[test]
fn test_hash_table_tracks_null_key_rows_and_indices() {
    let mut table = JoinHashTable::new(vec![0]);
    table.insert(Row::new(vec![Value::Int32(1), Value::Text("a".into())]));
    table.insert(Row::new(vec![Value::Null, Value::Text("null-key".into())]));
    table.insert(Row::new(vec![Value::Int32(2), Value::Text("b".into())]));
    table.finalize();

    assert_eq!(table.total_row_count(), 3);
    assert_eq!(table.null_key_start_index, 2);
    assert_eq!(table.null_key_rows.len(), 1);

    let hash = hash_join_key(&[Value::Int32(1)]);
    let (rows, indices) = table.bucket_by_hash(hash).expect("bucket exists");
    assert_eq!(rows.len(), 1);
    assert_eq!(indices, &[0]);

    let all: Vec<(usize, Value)> = table
        .all_rows_with_indices()
        .map(|(idx, r)| (idx, r.values[1].clone()))
        .collect();
    assert!(all
        .iter()
        .any(|(idx, v)| *idx == 2 && *v == Value::Text("null-key".into())));
}

#[test]
fn test_hash_table_into_unmatched_parts_preserves_components() {
    let mut table = JoinHashTable::with_capacity(vec![0], 8);
    table.insert(Row::new(vec![Value::Int32(1)]));
    table.insert(Row::new(vec![Value::Null]));
    table.finalize();

    let (buckets, null_rows, null_start) = table.into_unmatched_parts();
    assert!(!buckets.is_empty());
    assert_eq!(null_rows.len(), 1);
    assert_eq!(null_start, 1);
}

#[test]
fn test_hash_key_and_join_equality_for_misc_value_variants() {
    use crate::model::IntervalValue;
    use rust_decimal::Decimal;

    let values = vec![
        Value::Boolean(true),
        Value::Text("abc".into()),
        Value::Bytes(vec![1, 2, 3]),
        Value::Timestamp(1_700_000_000_000),
        Value::Interval(IntervalValue::new(2, 3456)),
        Value::Uuid([7u8; 16]),
        Value::Array(vec![Value::Int32(1), Value::Text("x".into())]),
        Value::Vector(vec![1.5, 2.5]),
        Value::Json("{\"a\":1}".into()),
        Value::Jsonb("{\"b\":2}".into()),
        Value::Time(12_345_678),
        Value::Date(20000),
        Value::Numeric(Decimal::new(12345, 2)),
        Value::Tsvector("'a' 'b'".into()),
        Value::Tsquery("a & b".into()),
    ];

    for v in values {
        assert_eq!(
            hash_join_key(std::slice::from_ref(&v)),
            hash_join_key(std::slice::from_ref(&v))
        );
        assert!(join_keys_equal(
            std::slice::from_ref(&v),
            std::slice::from_ref(&v)
        ));
    }
}

#[test]
fn test_row_key_equals_values_length_mismatch_returns_false() {
    let mut table = JoinHashTable::new(vec![0, 1]);
    table.insert(Row::new(vec![Value::Int32(1), Value::Int32(2)]));
    table.finalize();
    let hash = hash_join_key(&[Value::Int32(1), Value::Int32(2)]);
    let bucket = table.buckets.get(&hash).expect("bucket exists");
    assert!(!table.row_key_equals_values(&bucket.rows[0], &[Value::Int32(1)]));
}

#[test]
fn test_hash_join_metadata_and_children_ordering() {
    let left_child: BoxedOperator =
        Box::new(super::super::scan::TableScanOperator::new(schema_left()));
    let right_child: BoxedOperator =
        Box::new(super::super::scan::TableScanOperator::new(schema_right()));

    let op = HashJoinOperator::new(
        left_child,
        right_child,
        HashJoinType::Inner,
        vec![0],
        vec![0],
        true,
        None,
        HashJoinConfig::default(),
    );

    assert_eq!(op.name(), "HashJoin");
    assert_eq!(
        op.explain_info(),
        Some("type=INNER, left_is_build=true".to_string())
    );

    let children = op.children();
    assert_eq!(children.len(), 2);
    assert_eq!(children[0].schema().name, "left");
    assert_eq!(children[1].schema().name, "right");
}

#[test]
fn test_hash_join_children_ordering_when_right_is_build() {
    let left_child: BoxedOperator =
        Box::new(super::super::scan::TableScanOperator::new(schema_left()));
    let right_child: BoxedOperator =
        Box::new(super::super::scan::TableScanOperator::new(schema_right()));

    let mut op = HashJoinOperator::new(
        left_child,
        right_child,
        HashJoinType::Right,
        vec![0],
        vec![0],
        false,
        None,
        HashJoinConfig::default(),
    );

    assert_eq!(
        op.explain_info(),
        Some("type=RIGHT, left_is_build=false".to_string())
    );

    let children = op.children();
    assert_eq!(children.len(), 2);
    assert_eq!(children[0].schema().name, "left");
    assert_eq!(children[1].schema().name, "right");

    let children_mut = op.children_mut();
    assert_eq!(children_mut.len(), 2);
    assert_eq!(children_mut[0].schema().name, "left");
    assert_eq!(children_mut[1].schema().name, "right");
}
