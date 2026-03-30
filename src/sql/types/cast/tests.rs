//\! Tests for cast module.

use super::*;
use crate::sql::error::SqlError;
use rust_decimal::Decimal;

// ---- Float64 → Int32 ----
#[test]
fn explicit_float_to_int32_rounds() {
    let r = cast(Value::Float64(2.7), &DataType::Int32, CastContext::Explicit).unwrap();
    assert_eq!(r, Value::Int32(3));
}

#[test]
fn assignment_float_to_int32_rejects_fraction() {
    let r = cast(
        Value::Float64(2.7),
        &DataType::Int32,
        CastContext::Assignment,
    );
    assert!(r.is_err());
}

#[test]
fn assignment_float_to_int32_accepts_whole() {
    let r = cast(
        Value::Float64(2.0),
        &DataType::Int32,
        CastContext::Assignment,
    )
    .unwrap();
    assert_eq!(r, Value::Int32(2));
}

// ---- Float64 → Int64 ----
#[test]
fn explicit_float_to_int64_rounds() {
    let r = cast(Value::Float64(2.7), &DataType::Int64, CastContext::Explicit).unwrap();
    assert_eq!(r, Value::Int64(3));
}

#[test]
fn assignment_float_to_int64_falls_through() {
    let r = cast(
        Value::Float64(2.0),
        &DataType::Int64,
        CastContext::Assignment,
    );
    assert!(r.is_err());
}

// ---- Bool → Int32 ----
#[test]
fn explicit_bool_to_int32() {
    assert_eq!(
        cast(
            Value::Boolean(true),
            &DataType::Int32,
            CastContext::Explicit
        )
        .unwrap(),
        Value::Int32(1)
    );
    assert_eq!(
        cast(
            Value::Boolean(false),
            &DataType::Int32,
            CastContext::Explicit
        )
        .unwrap(),
        Value::Int32(0)
    );
}

#[test]
fn assignment_bool_to_int32_rejected() {
    let r = cast(
        Value::Boolean(true),
        &DataType::Int32,
        CastContext::Assignment,
    );
    assert!(r.is_err());
}

// ---- Int → Bool ----
#[test]
fn explicit_int_to_bool() {
    assert_eq!(
        cast(Value::Int32(0), &DataType::Boolean, CastContext::Explicit).unwrap(),
        Value::Boolean(false)
    );
    assert_eq!(
        cast(Value::Int32(42), &DataType::Boolean, CastContext::Explicit).unwrap(),
        Value::Boolean(true)
    );
}

#[test]
fn assignment_int_to_bool_rejected() {
    let r = cast(Value::Int32(1), &DataType::Boolean, CastContext::Assignment);
    assert!(r.is_err());
}

// ---- Numeric → Int32 ----
#[test]
fn explicit_numeric_to_int32_rounds() {
    let d = Decimal::new(27, 1); // 2.7
    let r = cast(Value::Numeric(d), &DataType::Int32, CastContext::Explicit).unwrap();
    assert_eq!(r, Value::Int32(3));
}

#[test]
fn assignment_numeric_to_int32_no_rounding() {
    // Assignment uses to_i32() directly which truncates toward zero
    let d = Decimal::new(27, 1); // 2.7 → truncates to 2
    let r = cast(Value::Numeric(d), &DataType::Int32, CastContext::Assignment).unwrap();
    assert_eq!(r, Value::Int32(2));

    let d2 = Decimal::new(20, 1); // 2.0
    let r2 = cast(
        Value::Numeric(d2),
        &DataType::Int32,
        CastContext::Assignment,
    )
    .unwrap();
    assert_eq!(r2, Value::Int32(2));
}

// ---- Numeric → Float64 ----
#[test]
fn explicit_numeric_to_float() {
    let d = Decimal::new(275, 2); // 2.75
    let r = cast(Value::Numeric(d), &DataType::Float64, CastContext::Explicit).unwrap();
    assert_eq!(r, Value::Float64(2.75));
}

#[test]
fn assignment_numeric_to_float() {
    let d = Decimal::new(275, 2); // 2.75
    let r = cast(
        Value::Numeric(d),
        &DataType::Float64,
        CastContext::Assignment,
    )
    .unwrap();
    assert_eq!(r, Value::Float64(2.75));
}

/// PG 17.7 parity: assignment numeric→float64 never produces NaN.
/// Decimal::MAX fits in f64, but the code path must return an error (not NaN)
/// if to_f64() ever returns None.
#[test]
fn assignment_numeric_to_float_never_produces_nan() {
    let d = Decimal::MAX;
    let r = cast(
        Value::Numeric(d),
        &DataType::Float64,
        CastContext::Assignment,
    )
    .unwrap();
    match r {
        Value::Float64(v) => assert!(!v.is_nan(), "assignment numeric→float must not produce NaN"),
        other => panic!("expected Float64, got {:?}", other),
    }
}

/// PG 17.7 parity: implicit numeric→float64 never produces NaN.
#[test]
fn implicit_numeric_to_float_never_produces_nan() {
    let d = Decimal::MAX;
    let r = cast(Value::Numeric(d), &DataType::Float64, CastContext::Implicit).unwrap();
    match r {
        Value::Float64(v) => assert!(!v.is_nan(), "implicit numeric→float must not produce NaN"),
        other => panic!("expected Float64, got {:?}", other),
    }
}

// ---- Unknown target: pass-through vs error ----
#[test]
fn explicit_unknown_target_passes_through() {
    let r = cast(
        Value::Boolean(true),
        &DataType::Tsvector,
        CastContext::Explicit,
    )
    .unwrap();
    assert_eq!(r, Value::Boolean(true));
}

#[test]
fn assignment_unknown_target_errors() {
    let r = cast(
        Value::Boolean(true),
        &DataType::Tsvector,
        CastContext::Assignment,
    );
    assert!(r.is_err());
}

// ---- Array: works in both contexts ----
#[test]
fn array_cast_both_contexts() {
    let arr = Value::Text("{1,2,3}".into());
    let target = DataType::Array(Box::new(DataType::Int32));

    let explicit = cast(arr.clone(), &target, CastContext::Explicit).unwrap();
    let assignment = cast(arr, &target, CastContext::Assignment).unwrap();

    let expected = Value::Array(vec![Value::Int32(1), Value::Int32(2), Value::Int32(3)]);
    assert_eq!(explicit, expected);
    assert_eq!(assignment, expected);
}

// ---- Vector: works in both contexts ----
#[test]
fn vector_cast_both_contexts() {
    let vec_val = Value::Text("[1.0,2.0,3.0]".into());
    let target = DataType::Vector(3);

    let explicit = cast(vec_val.clone(), &target, CastContext::Explicit).unwrap();
    let assignment = cast(vec_val, &target, CastContext::Assignment).unwrap();

    let expected = Value::Vector(vec![1.0, 2.0, 3.0]);
    assert_eq!(explicit, expected);
    assert_eq!(assignment, expected);
}

// ---- Null ----
#[test]
fn null_cast_any_context() {
    assert_eq!(
        cast(Value::Null, &DataType::Int32, CastContext::Explicit).unwrap(),
        Value::Null
    );
    assert_eq!(
        cast(Value::Null, &DataType::Int32, CastContext::Assignment).unwrap(),
        Value::Null
    );
}

// ---- Text conversions ----
#[test]
fn text_to_bool_both_contexts() {
    assert_eq!(
        cast(
            Value::Text("true".into()),
            &DataType::Boolean,
            CastContext::Explicit
        )
        .unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(
        cast(
            Value::Text("false".into()),
            &DataType::Boolean,
            CastContext::Assignment
        )
        .unwrap(),
        Value::Boolean(false)
    );
}

#[test]
fn text_to_bool_on_off() {
    assert_eq!(
        cast(
            Value::Text("on".into()),
            &DataType::Boolean,
            CastContext::Implicit
        )
        .unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(
        cast(
            Value::Text("OFF".into()),
            &DataType::Boolean,
            CastContext::Implicit
        )
        .unwrap(),
        Value::Boolean(false)
    );
}

#[test]
fn text_to_tsquery_validates_syntax() {
    let r = cast(
        Value::Text("'hello' & !'world'".into()),
        &DataType::Tsquery,
        CastContext::Explicit,
    )
    .unwrap();
    assert_eq!(r, Value::Tsquery("'hello' & !'world'".into()));
}

#[test]
fn text_to_tsquery_invalid_syntax_errors() {
    let err = cast(
        Value::Text("'hello' & (".into()),
        &DataType::Tsquery,
        CastContext::Explicit,
    )
    .unwrap_err();
    let sql_err = err
        .downcast_ref::<SqlError>()
        .expect("expected typed SqlError for tsquery syntax");
    assert_eq!(sql_err.sqlstate(), "42601");
    assert!(sql_err.to_string().contains("no operand in tsquery"));
}

#[test]
fn text_to_tsquery_syntax_error_leading_operator() {
    let err = cast(
        Value::Text("& foo".into()),
        &DataType::Tsquery,
        CastContext::Explicit,
    )
    .unwrap_err();
    let sql_err = err
        .downcast_ref::<SqlError>()
        .expect("expected typed SqlError for tsquery syntax");
    assert_eq!(sql_err.sqlstate(), "42601");
    assert!(sql_err.to_string().contains("syntax error in tsquery"));
}

#[test]
fn text_to_tsquery_syntax_error_double_operator() {
    let err = cast(
        Value::Text("'foo' && 'bar'".into()),
        &DataType::Tsquery,
        CastContext::Explicit,
    )
    .unwrap_err();
    let sql_err = err
        .downcast_ref::<SqlError>()
        .expect("expected typed SqlError for tsquery syntax");
    assert_eq!(sql_err.sqlstate(), "42601");
    assert!(sql_err.to_string().contains("syntax error in tsquery"));
}

#[test]
fn text_to_tsquery_syntax_error_leading_rparen() {
    let err = cast(
        Value::Text(") foo".into()),
        &DataType::Tsquery,
        CastContext::Explicit,
    )
    .unwrap_err();
    let sql_err = err
        .downcast_ref::<SqlError>()
        .expect("expected typed SqlError for tsquery syntax");
    assert_eq!(sql_err.sqlstate(), "42601");
    assert!(sql_err.to_string().contains("syntax error in tsquery"));
}

#[test]
fn text_to_empty_tsquery_is_valid() {
    let r = cast(
        Value::Text("".into()),
        &DataType::Tsquery,
        CastContext::Explicit,
    )
    .unwrap();
    assert_eq!(r, Value::Tsquery("".into()));
}

// ---- coerce_text_to_numeric ----
#[test]
fn coerce_text_to_numeric_int() {
    let v = coerce_text_to_numeric(Value::Text("42".into())).unwrap();
    assert_eq!(v, Value::Int64(42));
}

#[test]
fn coerce_text_to_numeric_float() {
    let v = coerce_text_to_numeric(Value::Text("3.14".into())).unwrap();
    #[allow(clippy::approx_constant)]
    let expected = Value::Float64(3.14);
    assert_eq!(v, expected);
}

#[test]
fn coerce_text_to_numeric_non_numeric_error() {
    let result = coerce_text_to_numeric(Value::Text("abc".into()));
    assert!(result.is_err());
}

#[test]
fn coerce_text_to_numeric_passthrough_non_text() {
    let v = coerce_text_to_numeric(Value::Int32(42)).unwrap();
    assert_eq!(v, Value::Int32(42));
}

#[test]
fn regclass_casts_normalize_to_int64_oid() {
    let regclass = DataType::UserDefined("pg_catalog.regclass".to_string());
    assert_eq!(
        cast(
            Value::Text("10000000001".into()),
            &regclass,
            CastContext::Explicit
        )
        .unwrap(),
        Value::Int64(10000000001)
    );
    assert_eq!(
        cast(Value::Int32(42), &regclass, CastContext::Implicit).unwrap(),
        Value::Int64(42)
    );
    assert_eq!(
        cast(Value::Int64(43), &regclass, CastContext::Assignment).unwrap(),
        Value::Int64(43)
    );
}

#[test]
fn regclass_text_catalog_name_resolves_to_oid() {
    let regclass = DataType::UserDefined("pg_catalog.regclass".to_string());
    // 'pg_class'::regclass → 1259 (used by JDBC getTables query)
    assert_eq!(
        cast(
            Value::Text("pg_class".into()),
            &regclass,
            CastContext::Explicit
        )
        .unwrap(),
        Value::Int64(1259)
    );
    // With schema prefix
    assert_eq!(
        cast(
            Value::Text("pg_catalog.pg_type".into()),
            &regclass,
            CastContext::Explicit
        )
        .unwrap(),
        Value::Int64(1247)
    );
    // Case-insensitive schema and relation names
    assert_eq!(
        cast(
            Value::Text("PG_CATALOG.PG_CLASS".into()),
            &regclass,
            CastContext::Explicit
        )
        .unwrap(),
        Value::Int64(1259)
    );
    // Unknown name should error
    assert!(cast(
        Value::Text("nonexistent_table".into()),
        &regclass,
        CastContext::Explicit
    )
    .is_err());
}

// ---- VARCHAR(n) ----
#[test]
fn explicit_varchar3_truncates() {
    let r = cast(
        Value::Text("hello".into()),
        &DataType::Varchar(3),
        CastContext::Explicit,
    )
    .unwrap();
    assert_eq!(r, Value::Text("hel".into()));
}

#[test]
fn assignment_varchar3_rejects_too_long() {
    let r = cast(
        Value::Text("hello".into()),
        &DataType::Varchar(3),
        CastContext::Assignment,
    );
    assert!(r.is_err());
    assert!(r
        .unwrap_err()
        .to_string()
        .contains("value too long for type character varying(3)"));
}

#[test]
fn assignment_varchar5_accepts_fits() {
    let r = cast(
        Value::Text("hello".into()),
        &DataType::Varchar(5),
        CastContext::Assignment,
    )
    .unwrap();
    assert_eq!(r, Value::Text("hello".into()));
}

#[test]
fn null_varchar_passthrough() {
    let r = cast(Value::Null, &DataType::Varchar(3), CastContext::Explicit).unwrap();
    assert_eq!(r, Value::Null);
}

#[test]
fn explicit_bare_varchar_does_not_truncate() {
    let r = cast(
        Value::Text("hello".into()),
        &DataType::Varchar(0),
        CastContext::Explicit,
    )
    .unwrap();
    assert_eq!(r, Value::Text("hello".into()));
}

#[test]
fn assignment_bare_varchar_accepts_unbounded_value() {
    let r = cast(
        Value::Text("hello".into()),
        &DataType::Varchar(0),
        CastContext::Assignment,
    )
    .unwrap();
    assert_eq!(r, Value::Text("hello".into()));
}

// ---- SQLSTATE roundtrip tests ----

#[test]
fn int64_to_int32_overflow_returns_22003() {
    let result = cast(
        Value::Int64(i64::MAX),
        &DataType::Int32,
        CastContext::Explicit,
    );
    let err = result.unwrap_err();
    let sql_err = err.downcast_ref::<SqlError>().expect("should be SqlError");
    assert_eq!(sql_err.sqlstate(), "22003");
    assert_eq!(err.to_string(), "integer out of range");
}

#[test]
fn float_nan_to_int32_returns_22003() {
    let result = cast(
        Value::Float64(f64::NAN),
        &DataType::Int32,
        CastContext::Explicit,
    );
    let err = result.unwrap_err();
    let sql_err = err.downcast_ref::<SqlError>().expect("should be SqlError");
    assert_eq!(sql_err.sqlstate(), "22003");
    assert_eq!(err.to_string(), "integer out of range");
}

#[test]
fn float_overflow_to_bigint_returns_22003() {
    let result = cast(
        Value::Float64(1e19),
        &DataType::Int64,
        CastContext::Explicit,
    );
    let err = result.unwrap_err();
    let sql_err = err.downcast_ref::<SqlError>().expect("should be SqlError");
    assert_eq!(sql_err.sqlstate(), "22003");
    assert_eq!(err.to_string(), "bigint out of range");
}

#[test]
fn varchar_truncation_assignment_returns_22001() {
    let result = cast(
        Value::Text("hello world".into()),
        &DataType::Varchar(3),
        CastContext::Assignment,
    );
    let err = result.unwrap_err();
    let sql_err = err.downcast_ref::<SqlError>().expect("should be SqlError");
    assert_eq!(sql_err.sqlstate(), "22001");
}

#[test]
fn varchar_explicit_truncates_silently() {
    let result = cast(
        Value::Text("hello world".into()),
        &DataType::Varchar(3),
        CastContext::Explicit,
    );
    assert_eq!(result.unwrap(), Value::Text("hel".into()));
}

// ---- JSONB → Text canonicalization ----
#[test]
fn jsonb_to_text_canonical() {
    let result = cast(
        Value::Jsonb(r#"{"b":1,"a":2}"#.into()),
        &DataType::Text,
        CastContext::Explicit,
    )
    .unwrap();
    assert_eq!(result, Value::Text(r#"{"a": 2, "b": 1}"#.into()));
}

// ---- JSONB → JSON canonicalization ----
#[test]
fn jsonb_to_json_canonical() {
    let result = cast(
        Value::Jsonb(r#"{"b":1,"a":2}"#.into()),
        &DataType::Json,
        CastContext::Explicit,
    )
    .unwrap();
    assert_eq!(result, Value::Json(r#"{"a": 2, "b": 1}"#.into()));
}

// ---- Tests moved from value_coercion.rs ----

use crate::model::ColumnDef;

fn test_col(name: &str, data_type: DataType) -> ColumnDef {
    ColumnDef::new(name, data_type, true)
}

#[test]
fn test_coerce_timestamp_time_array_vector_from_text() {
    let ts_col = test_col("ts", DataType::Timestamp);
    let got = coerce_value_for_column(Value::Text("2026-01-01 00:00:00".into()), &ts_col).unwrap();
    assert!(matches!(got, Value::Timestamp(_)));

    let tstz_col = test_col("tsz", DataType::TimestampTz);
    let got = coerce_value_for_column(
        Value::Text("2026-02-02 23:39:52.850 +00:00".into()),
        &tstz_col,
    )
    .unwrap();
    assert!(matches!(got, Value::Timestamp(_)));

    let ts_bad = coerce_value_for_column(Value::Text("not-a-ts".into()), &ts_col)
        .unwrap_err()
        .to_string();
    assert!(ts_bad.contains("invalid input syntax for type timestamp"));

    let time_col = test_col("t", DataType::Time);
    let got = coerce_value_for_column(Value::Text("01:02:03.004005".into()), &time_col).unwrap();
    assert_eq!(got, Value::Time(3_723_004_005));

    let time_bad = coerce_value_for_column(Value::Text("99:99".into()), &time_col)
        .unwrap_err()
        .to_string();
    assert!(time_bad.contains("invalid input syntax for type time"));

    let arr_col = test_col("a", DataType::Array(Box::new(DataType::Int32)));
    let got = coerce_value_for_column(Value::Text("{1,2}".into()), &arr_col).unwrap();
    assert_eq!(got, Value::Array(vec![Value::Int32(1), Value::Int32(2)]));

    let arr_bad = coerce_value_for_column(Value::Text("not-an-array".into()), &arr_col)
        .unwrap_err()
        .to_string();
    assert!(arr_bad.contains("invalid input syntax for type array"));

    let vec_col = test_col("v", DataType::Vector(3));
    let got = coerce_value_for_column(Value::Text("[1, 2, 3]".into()), &vec_col).unwrap();
    assert_eq!(got, Value::Vector(vec![1.0, 2.0, 3.0]));

    let vec_bad = coerce_value_for_column(Value::Text("not-a-vector".into()), &vec_col)
        .unwrap_err()
        .to_string();
    assert!(vec_bad.contains("invalid input syntax for type vector"));
}

#[test]
fn test_value_to_sql_expr_timestamp_preserves_timestamp_semantics() {
    use chrono::{TimeZone, Utc};
    use sqlparser::ast::BinaryOperator;

    let ts1 = Utc
        .with_ymd_and_hms(2000, 1, 1, 0, 0, 1)
        .single()
        .unwrap()
        .timestamp_millis();
    let ts2 = Utc
        .with_ymd_and_hms(2000, 1, 1, 0, 0, 2)
        .single()
        .unwrap()
        .timestamp_millis();

    let right_expr = value_to_sql_expr(&Value::Timestamp(ts1));
    let right_val = crate::sql::expr::bridge::eval_const_ast_expr(&right_expr).unwrap();

    let diff = crate::sql::expr::operators::eval_binary_op(
        Value::Timestamp(ts2),
        &BinaryOperator::Minus,
        right_val,
    )
    .unwrap();

    assert_eq!(
        diff,
        Value::Interval(crate::model::IntervalValue::from_millis(1_000))
    );
}

#[test]
fn test_parse_pg_array() {
    let result = parse_pg_array("{1,2,3}").unwrap();
    assert_eq!(result.len(), 3);
    assert_eq!(result[0], Value::Int32(1));
    assert_eq!(result[1], Value::Int32(2));
    assert_eq!(result[2], Value::Int32(3));
}

#[test]
fn test_parse_pg_array_empty() {
    let result = parse_pg_array("{}").unwrap();
    assert!(result.is_empty());
}

#[test]
fn test_parse_pg_array_strings() {
    let result = parse_pg_array("{hello,world}").unwrap();
    assert_eq!(result.len(), 2);
    assert_eq!(result[0], Value::Text("hello".to_string()));
    assert_eq!(result[1], Value::Text("world".to_string()));
}

#[test]
fn test_parse_pg_array_quoted_null_is_text() {
    let result = parse_pg_array("{NULL,\"NULL\"}").unwrap();
    assert_eq!(result.len(), 2);
    assert_eq!(result[0], Value::Null);
    assert_eq!(result[1], Value::Text("NULL".to_string()));
}

#[test]
fn test_infer_data_type() {
    assert_eq!(infer_data_type(&Value::Int32(1)), DataType::Int32);
    assert_eq!(infer_data_type(&Value::Int64(1)), DataType::Int64);
    assert_eq!(infer_data_type(&Value::Float64(1.0)), DataType::Float64);
    assert_eq!(infer_data_type(&Value::Boolean(true)), DataType::Boolean);
    assert_eq!(
        infer_data_type(&Value::Text("".to_string())),
        DataType::Text
    );
}
