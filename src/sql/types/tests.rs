use crate::types::DataType;

use super::*;

#[test]
fn test_function_registry_count() {
    let reg = global_registry();
    assert_eq!(
        reg.resolve_return_type("COUNT", &[DataType::Int32]),
        Some(DataType::Int64)
    );
    assert_eq!(reg.resolve_return_type("count", &[]), Some(DataType::Int64));
}

#[test]
fn test_function_registry_sum() {
    let reg = global_registry();
    assert_eq!(
        reg.resolve_return_type("SUM", &[DataType::Int32]),
        Some(DataType::Int64)
    );
    assert_eq!(
        reg.resolve_return_type("SUM", &[DataType::Float64]),
        Some(DataType::Float64)
    );
}

#[test]
fn test_function_registry_min_max() {
    let reg = global_registry();
    assert_eq!(
        reg.resolve_return_type("MIN", &[DataType::Text]),
        Some(DataType::Text)
    );
    assert_eq!(
        reg.resolve_return_type("MAX", &[DataType::Int64]),
        Some(DataType::Int64)
    );
}

#[test]
fn test_type_unification() {
    let types = vec![DataType::Int32, DataType::Int64];
    assert_eq!(unify_types(&types), Some(DataType::Int64));

    let types = vec![DataType::Int32, DataType::Float64];
    assert_eq!(unify_types(&types), Some(DataType::Float64));

    let types = vec![DataType::Text, DataType::Int32];
    assert_eq!(unify_types(&types), Some(DataType::Text));
}

#[test]
fn test_binary_op_types() {
    assert_eq!(
        binary_op_result_type("Plus", &DataType::Int32, &DataType::Int64),
        Some(DataType::Int64)
    );
    assert_eq!(
        binary_op_result_type("Minus", &DataType::Timestamp, &DataType::Timestamp),
        Some(DataType::Interval)
    );
    assert_eq!(
        binary_op_result_type("Eq", &DataType::Text, &DataType::Text),
        Some(DataType::Boolean)
    );
}

#[test]
fn test_is_numeric() {
    assert!(is_numeric(&DataType::Int32));
    assert!(is_numeric(&DataType::Int64));
    assert!(is_numeric(&DataType::Float64));
    assert!(is_numeric(&DataType::Numeric {
        precision: None,
        scale: None
    }));
    assert!(!is_numeric(&DataType::Text));
    assert!(!is_numeric(&DataType::Boolean));
}
