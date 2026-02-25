//! Tests for the encoding module.

use super::*;
use crate::model::{ColumnDef, DataType, Row, TableSchema, Value};

#[test]
fn test_encode_database_name_key() {
    let key = encode_database_name_key("mydb");
    assert_eq!(key, b"_sys_dbname_mydb".to_vec());
}

#[test]
fn test_encode_database_id_key() {
    let key = encode_database_id_key(1);
    assert_eq!(
        &key[..SYS_DATABASE_BY_ID_PREFIX.len()],
        SYS_DATABASE_BY_ID_PREFIX
    );
    assert_eq!(
        &key[SYS_DATABASE_BY_ID_PREFIX.len()..],
        &1_u64.to_be_bytes()
    );
}

#[test]
fn test_encode_database_data_prefix() {
    let key = encode_database_data_prefix(1);
    assert_eq!(key.len(), 11);
    assert_eq!(&key[..2], b"d_");
    assert_eq!(&key[2..10], &1_u64.to_be_bytes());
    assert_eq!(key[10], b'_');
}

#[test]
fn test_encode_migration_key() {
    let key = encode_migration_key("20260212_000001_init");
    assert_eq!(key, b"_sys_migration_20260212_000001_init".to_vec());
}

#[test]
fn test_encode_migration_prefix() {
    assert_eq!(encode_migration_prefix(), b"_sys_migration_".to_vec());
}

#[test]
fn test_encode_stats_key_v2() {
    let key = encode_stats_key_v2(1, 42);
    let mut expected = encode_database_data_prefix(1);
    expected.extend_from_slice(b"sys_stats_");
    expected.extend_from_slice(&42_u64.to_be_bytes());
    assert_eq!(key, expected);
}

#[test]
fn test_encode_stats_key_v2_different_tables() {
    let key_a = encode_stats_key_v2(1, 10);
    let key_b = encode_stats_key_v2(1, 20);
    assert_ne!(key_a, key_b);
    // Keys for same db should share the database prefix
    let prefix = encode_database_data_prefix(1);
    assert!(key_a.starts_with(&prefix));
    assert!(key_b.starts_with(&prefix));
}

#[test]
fn test_encode_stats_key_v2_different_databases() {
    let key_a = encode_stats_key_v2(1, 42);
    let key_b = encode_stats_key_v2(2, 42);
    assert_ne!(key_a, key_b);
}

#[test]
fn test_encode_database_data_range() {
    let (start, end) = encode_database_data_range(5);
    assert_eq!(start, encode_database_data_prefix(5));
    assert_eq!(end, encode_database_data_prefix(6));
    assert!(start < end);
}

#[test]
fn test_encode_schema_key_v2() {
    let key = encode_schema_key_v2(1, "public.users");
    let mut expected = encode_database_data_prefix(1);
    expected.extend_from_slice(b"sys_schema_public.users");
    assert_eq!(key, expected);
}

#[test]
fn test_encode_table_data_range_v2() {
    let (start, end) = encode_table_data_range_v2(5, 7);
    assert!(start < end);
    assert!(start.starts_with(&encode_database_data_prefix(5)));
    assert!(end.starts_with(&encode_database_data_prefix(5)));
}

#[test]
fn test_encode_pk_values_single() {
    let values = vec![Value::Int32(42)];
    let encoded = encode_pk_values(&values);
    let types = [DataType::Int32];
    let mut offset = 0;
    let (decoded, consumed) = decode_value_memcomparable(&encoded[offset..], &types[0]).unwrap();
    offset += consumed;
    assert_eq!(offset, encoded.len());
    assert_eq!(decoded, values[0]);
}

#[test]
fn test_encode_pk_values_composite() {
    let values = vec![Value::Int32(1), Value::Text("test".to_string())];
    let encoded = encode_pk_values(&values);
    let types = [DataType::Int32, DataType::Text];
    let mut offset = 0;
    let (v1, c1) = decode_value_memcomparable(&encoded[offset..], &types[0]).unwrap();
    offset += c1;
    let (v2, c2) = decode_value_memcomparable(&encoded[offset..], &types[1]).unwrap();
    offset += c2;
    assert_eq!(offset, encoded.len());
    assert_eq!(v1, values[0]);
    assert_eq!(v2, values[1]);
}

#[test]
fn test_serialize_deserialize_row() {
    let row = Row::new(vec![
        Value::Int32(1),
        Value::Text("hello".to_string()),
        Value::Boolean(true),
        Value::Null,
    ]);
    let serialized = serialize_row(&row).unwrap();
    let deserialized = deserialize_row(&serialized).unwrap();
    assert_eq!(deserialized.values, row.values);
}

#[test]
fn test_serialize_deserialize_numeric() {
    use rust_decimal::Decimal;
    use std::str::FromStr;
    let row = Row::new(vec![
        Value::Int32(1),
        Value::Numeric(Decimal::from_str("123.45").unwrap()),
        Value::Numeric(Decimal::from_str("-999.99").unwrap()),
        Value::Numeric(Decimal::ZERO),
    ]);
    let serialized = serialize_row(&row).unwrap();
    let deserialized = deserialize_row(&serialized).unwrap();
    assert_eq!(deserialized.values, row.values);
}

#[test]
fn test_serialize_deserialize_schema() {
    let schema = TableSchema {
        name: "test_table".to_string(),
        table_id: 42,
        columns: vec![
            ColumnDef {
                name: "id".to_string(),
                data_type: DataType::Int32,
                nullable: false,
                primary_key: true,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            },
            ColumnDef {
                name: "name".to_string(),
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
        pk_constraint_name: Some("test_table_pkey".to_string()),
        pk_indices: vec![0],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: "postgres".to_string(),
        from_alias: None,
    };
    let serialized = serialize_schema(&schema).unwrap();
    let deserialized = deserialize_schema(&serialized).unwrap();
    assert_eq!(deserialized.name, schema.name);
    assert_eq!(deserialized.table_id, schema.table_id);
    assert_eq!(deserialized.columns.len(), 2);
}

#[test]
fn test_memcomparable_int32_ordering() {
    let key_neg = encode_pk_values(&[Value::Int32(-100)]);
    let key_zero = encode_pk_values(&[Value::Int32(0)]);
    let key_pos = encode_pk_values(&[Value::Int32(100)]);
    assert!(key_neg < key_zero, "negative should be less than zero");
    assert!(key_zero < key_pos, "zero should be less than positive");
}

#[test]
fn test_memcomparable_int64_ordering() {
    let key_neg = encode_pk_values(&[Value::Int64(-1000)]);
    let key_zero = encode_pk_values(&[Value::Int64(0)]);
    let key_pos = encode_pk_values(&[Value::Int64(1000)]);
    assert!(key_neg < key_zero);
    assert!(key_zero < key_pos);
}

#[test]
fn test_memcomparable_text_ordering() {
    let key_a = encode_pk_values(&[Value::Text("apple".to_string())]);
    let key_b = encode_pk_values(&[Value::Text("banana".to_string())]);
    let key_c = encode_pk_values(&[Value::Text("cherry".to_string())]);
    assert!(key_a < key_b);
    assert!(key_b < key_c);
}

#[test]
fn test_memcomparable_null_ordering() {
    let key_null = encode_pk_values(&[Value::Null]);
    let key_value = encode_pk_values(&[Value::Int32(0)]);
    assert!(key_null < key_value, "NULL should sort before any value");
}

#[test]
fn test_memcomparable_float64_ordering() {
    let key_neg = encode_pk_values(&[Value::Float64(-1.5)]);
    let key_zero = encode_pk_values(&[Value::Float64(0.0)]);
    let key_pos = encode_pk_values(&[Value::Float64(1.5)]);
    assert!(key_neg < key_zero);
    assert!(key_zero < key_pos);
}

#[test]
fn test_memcomparable_numeric_ordering() {
    use rust_decimal::Decimal;
    use std::str::FromStr;

    let k_009 = encode_pk_values(&[Value::Numeric(Decimal::from_str("0.09").unwrap())]);
    let k_01 = encode_pk_values(&[Value::Numeric(Decimal::from_str("0.1").unwrap())]);
    let k_119 = encode_pk_values(&[Value::Numeric(Decimal::from_str("1.19").unwrap())]);
    let k_12 = encode_pk_values(&[Value::Numeric(Decimal::from_str("1.2").unwrap())]);
    assert!(k_009 < k_01);
    assert!(k_119 < k_12);

    let k_n100 = encode_pk_values(&[Value::Numeric(Decimal::from_str("-100").unwrap())]);
    let k_n2 = encode_pk_values(&[Value::Numeric(Decimal::from_str("-2").unwrap())]);
    assert!(k_n100 < k_n2);

    let k_zero = encode_pk_values(&[Value::Numeric(Decimal::ZERO)]);
    assert!(k_n2 < k_zero);
    assert!(k_zero < k_01);
}

#[test]
fn test_memcomparable_numeric_canonicalization() {
    use rust_decimal::Decimal;
    use std::str::FromStr;

    let a = encode_pk_values(&[Value::Numeric(Decimal::from_str("1.0").unwrap())]);
    let b = encode_pk_values(&[Value::Numeric(Decimal::from_str("1.00").unwrap())]);
    assert_eq!(a, b);
}

#[test]
fn test_decode_memcomparable_numeric() {
    use rust_decimal::Decimal;
    use std::str::FromStr;

    let values = vec![
        Value::Numeric(Decimal::from_str("100").unwrap()),
        Value::Numeric(Decimal::from_str("-0.09").unwrap()),
        Value::Numeric(Decimal::from_str("1.2").unwrap()),
    ];
    let ty = DataType::Numeric {
        precision: None,
        scale: None,
    };
    for value in values {
        let encoded = encode_pk_values(std::slice::from_ref(&value));
        let (decoded, consumed) = decode_value_memcomparable(&encoded, &ty).unwrap();
        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded, value);
    }
}

#[test]
fn test_encode_index_range_start_end_int32() {
    let start = encode_index_range_start_v2(1, 10, 20, &[], Some(&Value::Int32(10)), true);
    let end = encode_index_range_end_v2(1, 10, 20, &[], Some(&Value::Int32(20)), true);

    let key_before = encode_index_key_v2(1, 10, 20, &[Value::Int32(9)], None);
    let key_after = encode_index_key_v2(1, 10, 20, &[Value::Int32(21)], None);
    assert!(key_before < start);
    assert!(key_after >= end);

    for i in 10..=20 {
        let key = encode_index_key_v2(1, 10, 20, &[Value::Int32(i)], None);
        assert!(key >= start, "key should be >= range start for {i}");
        assert!(key < end, "key should be < range end for {i}");
    }
}

#[test]
fn test_encode_index_range_exclusive_bounds() {
    let start = encode_index_range_start_v2(1, 10, 20, &[], Some(&Value::Int32(10)), false);
    let end = encode_index_range_end_v2(1, 10, 20, &[], Some(&Value::Int32(20)), false);

    let key_10 = encode_index_key_v2(1, 10, 20, &[Value::Int32(10)], None);
    let key_11 = encode_index_key_v2(1, 10, 20, &[Value::Int32(11)], None);
    let key_19 = encode_index_key_v2(1, 10, 20, &[Value::Int32(19)], None);
    let key_20 = encode_index_key_v2(1, 10, 20, &[Value::Int32(20)], None);

    assert!(key_10 < start);
    assert!(key_11 >= start);
    assert!(key_19 < end);
    assert!(key_20 >= end);
}

#[test]
fn test_encode_index_range_with_prefix() {
    let start =
        encode_index_range_start_v2(1, 10, 20, &[Value::Int32(1)], Some(&Value::Int32(5)), true);

    let key_14 = encode_index_key_v2(1, 10, 20, &[Value::Int32(1), Value::Int32(4)], None);
    let key_15 = encode_index_key_v2(1, 10, 20, &[Value::Int32(1), Value::Int32(5)], None);
    let key_16 = encode_index_key_v2(1, 10, 20, &[Value::Int32(1), Value::Int32(6)], None);

    assert!(key_14 < start);
    assert!(key_15 >= start);
    assert!(start < key_16);
}

#[test]
fn test_encode_index_range_null_sorts_first() {
    let start = encode_index_range_start_v2(1, 10, 20, &[], Some(&Value::Int32(5)), false);

    let key_null = encode_index_key_v2(1, 10, 20, &[Value::Null], None);
    let key_5 = encode_index_key_v2(1, 10, 20, &[Value::Int32(5)], None);
    let key_6 = encode_index_key_v2(1, 10, 20, &[Value::Int32(6)], None);

    assert!(key_null < start);
    assert!(key_5 < start);
    assert!(key_6 >= start);
}

#[test]
fn test_encode_index_range_unbounded() {
    let start = encode_index_range_start_v2(1, 10, 20, &[Value::Int32(1)], None, true);
    let end = encode_index_range_end_v2(1, 10, 20, &[Value::Int32(1)], None, true);

    let key_low = encode_index_key_v2(1, 10, 20, &[Value::Int32(1), Value::Int32(-100)], None);
    let key_high = encode_index_key_v2(1, 10, 20, &[Value::Int32(1), Value::Int32(999)], None);

    assert_eq!(
        start,
        encode_index_key_v2(1, 10, 20, &[Value::Int32(1)], None)
    );
    assert!(start <= key_low);
    assert!(key_high < end);
}

#[test]
fn test_composite_key_ordering() {
    let key1 = encode_pk_values(&[Value::Int32(1), Value::Text("a".to_string())]);
    let key2 = encode_pk_values(&[Value::Int32(1), Value::Text("b".to_string())]);
    let key3 = encode_pk_values(&[Value::Int32(2), Value::Text("a".to_string())]);
    assert!(key1 < key2, "same first col, second col determines order");
    assert!(key2 < key3, "first col determines order");
}

// ========================================================================
// Worker System Key Tests
// ========================================================================

#[test]
fn test_encode_worker_registry_key() {
    let key = encode_worker_registry_key("myapp", 42);
    assert!(key.starts_with(WORKER_REGISTRY_PREFIX));
    // Verify keyspace length encoding
    let keyspace_len_bytes = &key[WORKER_REGISTRY_PREFIX.len()..WORKER_REGISTRY_PREFIX.len() + 2];
    let keyspace_len = u16::from_be_bytes([keyspace_len_bytes[0], keyspace_len_bytes[1]]);
    assert_eq!(keyspace_len, 5); // "myapp" is 5 bytes
}

#[test]
fn test_encode_worker_registry_prefix() {
    let prefix = encode_worker_registry_prefix();
    assert_eq!(prefix, WORKER_REGISTRY_PREFIX);
}

#[test]
fn test_encode_worker_queue_key() {
    let key = encode_worker_queue_key(10, 1000, "myapp", 42, 100);
    assert!(key.starts_with(WORKER_QUEUE_PREFIX));
    // Verify priority byte is at correct position
    let priority_byte = key[WORKER_QUEUE_PREFIX.len()];
    assert_eq!(priority_byte, 10);
}

#[test]
fn test_worker_queue_key_priority_ordering() {
    let key_p0 = encode_worker_queue_key(0, 1000, "myapp", 42, 100);
    let key_p128 = encode_worker_queue_key(128, 1000, "myapp", 42, 100);
    assert!(key_p0 < key_p128, "lower priority value should sort first");
}

#[test]
fn test_worker_queue_key_fire_time_ordering() {
    let key_t1000 = encode_worker_queue_key(0, 1000, "myapp", 42, 100);
    let key_t2000 = encode_worker_queue_key(0, 2000, "myapp", 42, 100);
    assert!(key_t1000 < key_t2000, "earlier fire_time should sort first");
}

#[test]
fn test_encode_worker_queue_prefix() {
    let prefix = encode_worker_queue_prefix();
    assert_eq!(prefix, WORKER_QUEUE_PREFIX);
}

#[test]
fn test_encode_worker_queue_scan_end() {
    let scan_end = encode_worker_queue_scan_end(10, 1000);
    assert!(scan_end.starts_with(WORKER_QUEUE_PREFIX));
    let priority_byte = scan_end[WORKER_QUEUE_PREFIX.len()];
    assert_eq!(priority_byte, 10);
}

#[test]
fn test_decode_worker_queue_fire_time() {
    let key = encode_worker_queue_key(10, 1234567890, "myapp", 42, 100);
    let fire_time = decode_worker_queue_fire_time(&key).expect("decode");
    assert_eq!(fire_time, 1234567890);
}

#[test]
fn test_decode_worker_queue_fire_time_roundtrip() {
    let original_time = 9876543210i64;
    let key = encode_worker_queue_key(5, original_time, "test", 1, 50);
    let decoded_time = decode_worker_queue_fire_time(&key).expect("decode");
    assert_eq!(decoded_time, original_time);
}

#[test]
fn test_decode_worker_queue_fire_time_invalid_key() {
    let short_key = b"_worker_queue_";
    assert_eq!(decode_worker_queue_fire_time(short_key), None);
}

#[test]
fn test_encode_worker_claim_key() {
    let key = encode_worker_claim_key("myapp", 42, 100, 5000);
    assert!(key.starts_with(WORKER_CLAIM_PREFIX));
    // Verify keyspace length encoding
    let keyspace_len_bytes = &key[WORKER_CLAIM_PREFIX.len()..WORKER_CLAIM_PREFIX.len() + 2];
    let keyspace_len = u16::from_be_bytes([keyspace_len_bytes[0], keyspace_len_bytes[1]]);
    assert_eq!(keyspace_len, 5); // "myapp" is 5 bytes
}

#[test]
fn test_encode_worker_claim_prefix() {
    let prefix = encode_worker_claim_prefix();
    assert_eq!(prefix, WORKER_CLAIM_PREFIX);
}

#[test]
fn test_worker_queue_key_priority_before_time() {
    // Higher priority (lower byte value) at later time should sort before lower priority at earlier time.
    // This proves that priority byte comes BEFORE fire_time in the key encoding.
    let key_high_late = encode_worker_queue_key(0, 2000, "ks", 1, 1);
    let key_low_early = encode_worker_queue_key(128, 1000, "ks", 1, 1);
    assert!(
        key_high_late < key_low_early,
        "priority must take precedence over fire_time"
    );
}

#[test]
fn test_worker_queue_key_big_endian_fire_time() {
    let key_neg = encode_worker_queue_key(0, -1000i64, "app", 1, 1);
    let key_zero = encode_worker_queue_key(0, 0i64, "app", 1, 1);
    let key_pos = encode_worker_queue_key(0, 1000i64, "app", 1, 1);
    assert!(key_neg < key_zero, "negative fire_time should sort first");
    assert!(key_zero < key_pos, "zero should sort before positive");
}

#[test]
fn test_worker_registry_key_different_keyspaces() {
    let key_a = encode_worker_registry_key("app_a", 42);
    let key_b = encode_worker_registry_key("app_b", 42);
    assert_ne!(key_a, key_b);
}

#[test]
fn test_worker_registry_key_different_db_ids() {
    let key_1 = encode_worker_registry_key("myapp", 1);
    let key_2 = encode_worker_registry_key("myapp", 2);
    assert_ne!(key_1, key_2);
}

#[test]
fn test_worker_claim_key_different_task_ids() {
    let key_1 = encode_worker_claim_key("myapp", 42, 100, 5000);
    let key_2 = encode_worker_claim_key("myapp", 42, 200, 5000);
    assert_ne!(key_1, key_2);
}

#[test]
fn test_worker_registry_key_roundtrip_prefix() {
    let key = encode_worker_registry_key("tenant_x", 7);
    assert!(key.starts_with(b"_worker_registry_"));
    let prefix = encode_worker_registry_prefix();
    assert!(key.starts_with(&prefix));
}

#[test]
fn test_worker_claim_key_structure() {
    let key = encode_worker_claim_key("demo", 10, 555, 9999);
    assert!(key.starts_with(b"_worker_claim_"));
    let prefix = encode_worker_claim_prefix();
    assert!(key.starts_with(&prefix));
}

#[test]
fn test_worker_queue_key_same_time_same_priority_different_keyspace() {
    let key_a = encode_worker_queue_key(5, 1000, "ks_alpha", 1, 1);
    let key_b = encode_worker_queue_key(5, 1000, "ks_beta", 1, 1);
    assert_ne!(key_a, key_b);
}

#[test]
fn test_worker_bg_result_key_structure() {
    let key = encode_worker_bg_result_key("myks", 3, 42);
    assert!(key.starts_with(b"_worker_bg_result_"));
    let ks_len_offset = WORKER_BG_RESULT_PREFIX.len();
    let ks_len = u16::from_be_bytes([key[ks_len_offset], key[ks_len_offset + 1]]);
    assert_eq!(ks_len, 4);
}

#[test]
fn test_worker_queue_scan_end_boundary() {
    let scan_end = encode_worker_queue_scan_end(5, 2000);
    let key_before = encode_worker_queue_key(5, 1999, "ks", 1, 1);
    let key_at = encode_worker_queue_key(5, 2000, "ks", 1, 1);
    assert!(
        key_before < scan_end,
        "key with earlier fire_time should be < scan_end"
    );
    assert!(
        key_at >= scan_end,
        "key at fire_time should be >= scan_end (scan_end is prefix)"
    );
}
