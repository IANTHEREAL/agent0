use crate::model::DataType;

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
fn test_function_registry_sign_matches_pg_return_types() {
    let reg = global_registry();
    assert_eq!(
        reg.resolve_return_type("SIGN", &[DataType::Int32]),
        Some(DataType::Float64)
    );
    assert_eq!(
        reg.resolve_return_type("SIGN", &[DataType::Int64]),
        Some(DataType::Float64)
    );
    assert_eq!(
        reg.resolve_return_type("SIGN", &[DataType::Float64]),
        Some(DataType::Float64)
    );
    assert_eq!(
        reg.resolve_return_type(
            "SIGN",
            &[DataType::Numeric {
                precision: Some(5),
                scale: Some(2),
            }]
        ),
        Some(DataType::Numeric {
            precision: None,
            scale: None,
        })
    );
}

#[test]
fn test_function_registry_mod_matches_pg_overloads() {
    let reg = global_registry();
    let numeric = DataType::Numeric {
        precision: None,
        scale: None,
    };
    assert_eq!(
        reg.resolve_return_type("MOD", &[DataType::Int32, DataType::Int32]),
        Some(DataType::Int32)
    );
    assert_eq!(
        reg.resolve_return_type("MOD", &[DataType::Int32, DataType::Int64]),
        Some(DataType::Int64)
    );
    assert_eq!(
        reg.resolve_return_type("MOD", &[numeric.clone(), numeric.clone()]),
        Some(numeric)
    );
    assert_eq!(
        reg.resolve_return_type("MOD", &[DataType::Float64, DataType::Float64]),
        None
    );
}

#[test]
fn test_ts_rank_registry_supports_pg_overloads() {
    let reg = global_registry();
    let ts_rank = reg.get("TS_RANK").expect("TS_RANK must exist");
    assert_eq!(ts_rank.min_args, 2);
    assert_eq!(ts_rank.max_args, Some(4));

    let ts_rank_cd = reg.get("TS_RANK_CD").expect("TS_RANK_CD must exist");
    assert_eq!(ts_rank_cd.min_args, 2);
    assert_eq!(ts_rank_cd.max_args, Some(4));
}

#[test]
fn test_embedding_registry_signatures() {
    let reg = global_registry();

    let embedding = reg.get("EMBEDDING").expect("EMBEDDING must exist");
    assert_eq!(embedding.min_args, 1);
    assert_eq!(embedding.max_args, Some(3));
    assert_eq!(
        reg.resolve_return_type("EMBEDDING", &[DataType::Text]),
        Some(DataType::Vector(0))
    );

    let embed_text = reg.get("EMBED_TEXT").expect("EMBED_TEXT must exist");
    assert_eq!(embed_text.min_args, 2);
    assert_eq!(embed_text.max_args, Some(3));
    assert_eq!(
        reg.resolve_return_type("EMBED_TEXT", &[DataType::Text, DataType::Text]),
        Some(DataType::Vector(0))
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
