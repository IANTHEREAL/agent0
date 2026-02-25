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
    assert!(table.buckets.get(&hash).is_none());
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
