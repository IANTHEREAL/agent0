//! Unit tests for the DML sub-modules.

use super::*;
use crate::model::{ColumnDef, DataType, Row, TableSchema, Value};
use std::collections::HashMap;

fn enum_schema() -> TableSchema {
    TableSchema::new(
        "t".to_string(),
        1,
        vec![ColumnDef::new(
            "r",
            DataType::UserDefined("public.role".to_string()),
            true,
        )],
        vec![],
    )
}

fn enum_cache() -> EnumLabelCache {
    let mut cache: EnumLabelCache = HashMap::new();
    cache.insert(
        "public.role".to_string(),
        ["USER", "ADMIN"]
            .into_iter()
            .map(|s| s.to_string())
            .collect(),
    );
    cache
}

#[test]
fn enum_validation_allows_null() {
    let schema = enum_schema();
    let cache = enum_cache();
    let row = Row::new(vec![Value::Null]);
    insert::validate_enum_values(&schema, &row, &cache).unwrap();
}

#[test]
fn enum_validation_rejects_unknown_label() {
    let schema = enum_schema();
    let cache = enum_cache();
    let row = Row::new(vec![Value::Text("INVALID".to_string())]);
    let err = insert::validate_enum_values(&schema, &row, &cache).unwrap_err();
    assert!(err
        .to_string()
        .contains("invalid input value for enum role"));
}

#[test]
fn enum_validation_is_case_sensitive() {
    let schema = enum_schema();
    let cache = enum_cache();
    let row = Row::new(vec![Value::Text("user".to_string())]);
    let err = insert::validate_enum_values(&schema, &row, &cache).unwrap_err();
    assert!(err
        .to_string()
        .contains("invalid input value for enum role"));
}

#[test]
fn enum_validation_requires_text() {
    let schema = enum_schema();
    let cache = enum_cache();
    let row = Row::new(vec![Value::Int32(1)]);
    let err = insert::validate_enum_values(&schema, &row, &cache).unwrap_err();
    assert!(err
        .to_string()
        .contains("invalid input value for enum role"));
}
