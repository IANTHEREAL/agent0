//! Tests for the typed expression evaluator.

use super::*;
use crate::model::DataType;
use crate::sql::error::SqlError;
use crate::sql::types::CastContext;
use std::collections::HashMap;
use std::sync::Arc;

fn make_row(vals: Vec<Value>) -> Row {
    Row { values: vals }
}

fn empty_row() -> Row {
    Row { values: vec![] }
}

fn test_qctx() -> QueryContext {
    QueryContext::new(
        1,                     // connection_id
        Arc::from("postgres"), // database_name
        Arc::from("postgres"), // current_user
        1_700_000_000_000,     // statement_timestamp_ms
        1_700_000_000_000,     // transaction_timestamp_ms
        Arc::from("UTC"),      // timezone
    )
}

fn const_expr(val: Value, dt: DataType) -> TypedExpr {
    TypedExpr::new(TypedExprKind::Constant(val), dt)
}

fn builtin_call(name: &str, return_type: DataType, args: Vec<TypedExpr>) -> TypedExpr {
    TypedExpr::new(
        TypedExprKind::FunctionCall {
            func: ResolvedFunction {
                name: name.into(),
                kind: FunctionKind::Builtin,
                return_type: return_type.clone(),
            },
            args,
            order_by: vec![],
            filter: None,
        },
        return_type,
    )
}

fn col_ref(index: usize, name: &str, dt: DataType) -> TypedExpr {
    TypedExpr::new(
        TypedExprKind::ColumnRef {
            scope_depth: 0,
            column_index: index,
            column_name: name.to_string(),
        },
        dt,
    )
}

fn bad_chr_expr() -> TypedExpr {
    builtin_call(
        "chr",
        DataType::Text,
        vec![const_expr(Value::Int32(-1), DataType::Int32)],
    )
}

fn bad_chr_predicate() -> TypedExpr {
    TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(bad_chr_expr()),
            op: BinaryOp::Eq,
            right: Box::new(const_expr(Value::Text(String::new()), DataType::Text)),
        },
        DataType::Boolean,
    )
}

// ── Constant ────────────────────────────────────────────

#[test]
fn test_constant_values() {
    let row = empty_row();
    let qctx = test_qctx();

    let expr = const_expr(Value::Int64(42), DataType::Int64);
    assert_eq!(
        eval_typed_expr(&expr, &row, &qctx).unwrap(),
        Value::Int64(42)
    );

    let expr = const_expr(Value::Text("hello".into()), DataType::Text);
    assert_eq!(
        eval_typed_expr(&expr, &row, &qctx).unwrap(),
        Value::Text("hello".into())
    );

    let expr = const_expr(Value::Null, DataType::Text);
    assert_eq!(eval_typed_expr(&expr, &row, &qctx).unwrap(), Value::Null);

    let expr = const_expr(Value::Boolean(true), DataType::Boolean);
    assert_eq!(
        eval_typed_expr(&expr, &row, &qctx).unwrap(),
        Value::Boolean(true)
    );
}

// ── Context-dependent builtins ──────────────────────────

#[test]
fn typed_builtin_timestamps_use_query_context() {
    let row = empty_row();
    let qctx = QueryContext::new(
        42,
        Arc::from("mydb"),
        Arc::from("postgres"),
        1_700_000_000_111,
        1_700_000_000_222,
        Arc::from("UTC"),
    );

    let now = func_call("NOW", vec![], DataType::TimestampTz);
    assert_eq!(
        eval_typed_expr(&now, &row, &qctx).unwrap(),
        Value::Timestamp(1_700_000_000_222)
    );

    let current_ts = func_call("CURRENT_TIMESTAMP", vec![], DataType::TimestampTz);
    assert_eq!(
        eval_typed_expr(&current_ts, &row, &qctx).unwrap(),
        Value::Timestamp(1_700_000_000_222)
    );

    let statement_ts = func_call("STATEMENT_TIMESTAMP", vec![], DataType::TimestampTz);
    assert_eq!(
        eval_typed_expr(&statement_ts, &row, &qctx).unwrap(),
        Value::Timestamp(1_700_000_000_111)
    );

    let transaction_ts = func_call("TRANSACTION_TIMESTAMP", vec![], DataType::TimestampTz);
    assert_eq!(
        eval_typed_expr(&transaction_ts, &row, &qctx).unwrap(),
        Value::Timestamp(1_700_000_000_222)
    );
}

#[test]
fn typed_builtin_current_date_uses_query_context() {
    let row = empty_row();
    let qctx = QueryContext::new(
        1,
        Arc::from("postgres"),
        Arc::from("postgres"),
        1_700_000_000_000,
        1_700_000_000_123,
        Arc::from("UTC"),
    );

    let expr = func_call("CURRENT_DATE", vec![], DataType::Date);
    let expected =
        crate::model::date::timestamp_millis_to_date_days(qctx.transaction_timestamp_ms).unwrap();
    assert_eq!(
        eval_typed_expr(&expr, &row, &qctx).unwrap(),
        Value::Date(expected)
    );
}

#[test]
fn typed_builtin_current_date_uses_session_timezone() {
    let row = empty_row();
    let qctx = QueryContext::new(
        1,
        Arc::from("postgres"),
        Arc::from("postgres"),
        1_704_151_800_000,
        1_704_151_800_000,
        Arc::from("Asia/Shanghai"),
    );

    let expr = func_call("CURRENT_DATE", vec![], DataType::Date);
    assert_eq!(
        eval_typed_expr(&expr, &row, &qctx).unwrap(),
        Value::Date(crate::model::date::parse_date_days("2024-01-02").unwrap())
    );
}

#[test]
fn typed_builtin_current_time_uses_query_context_timezone() {
    let row = empty_row();
    let qctx = QueryContext::new(
        1,
        Arc::from("postgres"),
        Arc::from("postgres"),
        1_700_000_000_000,
        1_700_000_000_123,
        Arc::from("Asia/Shanghai"),
    );

    let expr = func_call("CURRENT_TIME", vec![], DataType::Time);
    let expected_millis =
        (qctx.transaction_timestamp_ms + 8 * 60 * 60 * 1000).rem_euclid(24 * 60 * 60 * 1000);
    assert_eq!(
        eval_typed_expr(&expr, &row, &qctx).unwrap(),
        Value::Time(expected_millis * 1000)
    );
}

#[test]
fn typed_builtin_localtime_aliases_current_time() {
    let row = empty_row();
    let qctx = QueryContext::new(
        1,
        Arc::from("postgres"),
        Arc::from("postgres"),
        1_700_000_000_000,
        1_700_000_000_123,
        Arc::from("Asia/Shanghai"),
    );

    let expr = func_call("LOCALTIME", vec![], DataType::Time);
    let expected_millis =
        (qctx.transaction_timestamp_ms + 8 * 60 * 60 * 1000).rem_euclid(24 * 60 * 60 * 1000);
    assert_eq!(
        eval_typed_expr(&expr, &row, &qctx).unwrap(),
        Value::Time(expected_millis * 1000)
    );
}

#[test]
fn typed_builtin_date_timestamptz_uses_session_timezone() {
    let row = empty_row();
    let qctx = QueryContext::new(
        1,
        Arc::from("postgres"),
        Arc::from("postgres"),
        1_700_000_000_000,
        1_700_000_000_000,
        Arc::from("Asia/Shanghai"),
    );
    let Value::Timestamp(ts_tz) =
        crate::sql::expr::parse_timestamp_string("2024-01-01 23:30:00+00").unwrap()
    else {
        panic!("timestamptz literal should parse as Value::Timestamp");
    };

    let expr = func_call(
        "date",
        vec![const_expr(Value::Timestamp(ts_tz), DataType::TimestampTz)],
        DataType::Date,
    );
    assert_eq!(
        eval_typed_expr(&expr, &row, &qctx).unwrap(),
        Value::Date(crate::model::date::parse_date_days("2024-01-02").unwrap())
    );
}

#[test]
fn typed_builtin_age_timestamptz_uses_session_timezone() {
    let row = empty_row();
    let qctx = QueryContext::new(
        1,
        Arc::from("postgres"),
        Arc::from("postgres"),
        1_700_000_000_000,
        1_700_000_000_000,
        Arc::from("Asia/Shanghai"),
    );
    let Value::Timestamp(end_ts) =
        crate::sql::expr::parse_timestamp_string("2024-03-01 00:30:00+08").unwrap()
    else {
        panic!("timestamptz literal should parse as Value::Timestamp");
    };
    let Value::Timestamp(start_ts) =
        crate::sql::expr::parse_timestamp_string("2024-02-01 00:30:00+08").unwrap()
    else {
        panic!("timestamptz literal should parse as Value::Timestamp");
    };

    let expr = func_call(
        "age",
        vec![
            const_expr(Value::Timestamp(end_ts), DataType::TimestampTz),
            const_expr(Value::Timestamp(start_ts), DataType::TimestampTz),
        ],
        DataType::Interval,
    );
    assert_eq!(
        eval_typed_expr(&expr, &row, &qctx).unwrap(),
        Value::Interval(crate::model::IntervalValue::new(1, 0))
    );
}

#[test]
fn typed_builtin_to_char_supports_temporal_overloads() {
    let row = empty_row();
    let qctx = QueryContext::new(
        1,
        Arc::from("postgres"),
        Arc::from("postgres"),
        1_700_000_000_000,
        1_700_000_000_000,
        Arc::from("Asia/Shanghai"),
    );
    let Value::Timestamp(ts_tz) =
        crate::sql::expr::parse_timestamp_string("2024-01-02 00:00:00+00").unwrap()
    else {
        panic!("timestamptz literal should parse as Value::Timestamp");
    };

    let time_expr = func_call(
        "TO_CHAR",
        vec![
            const_expr(
                Value::Time(12 * 3_600_000_000 + 34 * 60 * 1_000_000 + 56 * 1_000_000),
                DataType::Time,
            ),
            const_expr(Value::Text("HH24:MI:SS".into()), DataType::Text),
        ],
        DataType::Text,
    );
    assert_eq!(
        eval_typed_expr(&time_expr, &row, &qctx).unwrap(),
        Value::Text("12:34:56".into())
    );

    let interval_expr = func_call(
        "TO_CHAR",
        vec![
            const_expr(
                Value::Interval(crate::model::IntervalValue::new(0, 3_723_000)),
                DataType::Interval,
            ),
            const_expr(Value::Text("HH24:MI:SS".into()), DataType::Text),
        ],
        DataType::Text,
    );
    assert_eq!(
        eval_typed_expr(&interval_expr, &row, &qctx).unwrap(),
        Value::Text("01:02:03".into())
    );

    let timestamptz_expr = func_call(
        "TO_CHAR",
        vec![
            const_expr(Value::Timestamp(ts_tz), DataType::TimestampTz),
            const_expr(Value::Text("YYYY-MM-DD HH24:MI:SS".into()), DataType::Text),
        ],
        DataType::Text,
    );
    assert_eq!(
        eval_typed_expr(&timestamptz_expr, &row, &qctx).unwrap(),
        Value::Text("2024-01-02 08:00:00".into())
    );
}

#[test]
fn typed_builtin_date_part_extract_and_trunc_use_session_timezone_for_timestamptz() {
    let row = empty_row();
    let qctx = QueryContext::new(
        1,
        Arc::from("postgres"),
        Arc::from("postgres"),
        1_700_000_000_000,
        1_700_000_000_000,
        Arc::from("Asia/Shanghai"),
    );
    let Value::Timestamp(ts_tz) =
        crate::sql::expr::parse_timestamp_string("2024-01-01 23:30:00+00").unwrap()
    else {
        panic!("timestamptz literal should parse as Value::Timestamp");
    };

    let date_part_day = func_call(
        "DATE_PART",
        vec![
            const_expr(Value::Text("day".into()), DataType::Text),
            const_expr(Value::Timestamp(ts_tz), DataType::TimestampTz),
        ],
        DataType::Float64,
    );
    assert_eq!(
        eval_typed_expr(&date_part_day, &row, &qctx).unwrap(),
        Value::Float64(2.0)
    );

    let extract_epoch = func_call(
        "EXTRACT",
        vec![
            const_expr(Value::Text("epoch".into()), DataType::Text),
            const_expr(Value::Timestamp(ts_tz), DataType::TimestampTz),
        ],
        DataType::Numeric {
            precision: None,
            scale: None,
        },
    );
    assert_eq!(
        eval_typed_expr(&extract_epoch, &row, &qctx).unwrap(),
        Value::Numeric(rust_decimal::Decimal::from_i128_with_scale(
            1_704_151_800_000_000,
            6
        ))
    );

    let date_trunc_day = func_call(
        "DATE_TRUNC",
        vec![
            const_expr(Value::Text("day".into()), DataType::Text),
            const_expr(Value::Timestamp(ts_tz), DataType::TimestampTz),
        ],
        DataType::TimestampTz,
    );
    let Value::Timestamp(expected) =
        crate::sql::expr::parse_timestamp_string("2024-01-01 16:00:00+00").unwrap()
    else {
        panic!("expected timestamptz literal should parse as Value::Timestamp");
    };
    assert_eq!(
        eval_typed_expr(&date_trunc_day, &row, &qctx).unwrap(),
        Value::Timestamp(expected)
    );
}

#[test]
fn typed_builtin_localtimestamp_uses_query_context_timezone() {
    let row = empty_row();
    let qctx = QueryContext::new(
        1,
        Arc::from("postgres"),
        Arc::from("postgres"),
        1_700_000_000_000,
        1_700_000_000_123,
        Arc::from("Asia/Shanghai"),
    );

    let expr = func_call("LOCALTIMESTAMP", vec![], DataType::Timestamp);
    assert_eq!(
        eval_typed_expr(&expr, &row, &qctx).unwrap(),
        Value::Timestamp(qctx.transaction_timestamp_ms + 8 * 60 * 60 * 1000)
    );
}

#[test]
fn typed_builtin_make_date_constructs_date() {
    let row = empty_row();
    let qctx = test_qctx();
    let expr = func_call(
        "MAKE_DATE",
        vec![
            const_expr(Value::Int32(2024), DataType::Int32),
            const_expr(Value::Int32(1), DataType::Int32),
            const_expr(Value::Int32(2), DataType::Int32),
        ],
        DataType::Date,
    );

    assert_eq!(
        eval_typed_expr(&expr, &row, &qctx).unwrap(),
        Value::Date(19_724)
    );
}

#[test]
fn typed_builtin_make_time_constructs_time() {
    let row = empty_row();
    let qctx = test_qctx();
    let expr = func_call(
        "MAKE_TIME",
        vec![
            const_expr(Value::Int32(8), DataType::Int32),
            const_expr(Value::Int32(15), DataType::Int32),
            const_expr(Value::Float64(23.5), DataType::Float64),
        ],
        DataType::Time,
    );

    assert_eq!(
        eval_typed_expr(&expr, &row, &qctx).unwrap(),
        Value::Time(29_723_500_000)
    );
}

#[test]
fn typed_builtin_make_timestamp_constructs_timestamp() {
    let row = empty_row();
    let qctx = test_qctx();
    let expr = func_call(
        "MAKE_TIMESTAMP",
        vec![
            const_expr(Value::Int32(2024), DataType::Int32),
            const_expr(Value::Int32(1), DataType::Int32),
            const_expr(Value::Int32(2), DataType::Int32),
            const_expr(Value::Int32(8), DataType::Int32),
            const_expr(Value::Int32(15), DataType::Int32),
            const_expr(Value::Float64(23.5), DataType::Float64),
        ],
        DataType::Timestamp,
    );
    let expected = chrono::NaiveDate::from_ymd_opt(2024, 1, 2)
        .unwrap()
        .and_hms_micro_opt(8, 15, 23, 500_000)
        .unwrap()
        .and_utc()
        .timestamp_millis();

    assert_eq!(
        eval_typed_expr(&expr, &row, &qctx).unwrap(),
        Value::Timestamp(expected)
    );
}

#[test]
fn typed_builtin_make_interval_accepts_numeric_seconds() {
    let row = empty_row();
    let qctx = test_qctx();
    let expr = func_call(
        "MAKE_INTERVAL",
        vec![
            const_expr(Value::Int32(0), DataType::Int32),
            const_expr(Value::Int32(0), DataType::Int32),
            const_expr(Value::Int32(0), DataType::Int32),
            const_expr(Value::Int32(2), DataType::Int32),
            const_expr(Value::Int32(3), DataType::Int32),
            const_expr(Value::Int32(4), DataType::Int32),
            const_expr(
                Value::Numeric(rust_decimal::Decimal::new(55, 1)),
                DataType::Numeric {
                    precision: None,
                    scale: None,
                },
            ),
        ],
        DataType::Interval,
    );

    assert_eq!(
        eval_typed_expr(&expr, &row, &qctx).unwrap(),
        Value::Interval(crate::model::IntervalValue::new(0, 183_845_500))
    );
}

#[test]
fn typed_builtin_to_timestamp_constructs_timestamptz_from_epoch_seconds() {
    let row = empty_row();
    let qctx = test_qctx();
    let expr = func_call(
        "TO_TIMESTAMP",
        vec![const_expr(
            Value::Float64(1_704_187_323.5),
            DataType::Float64,
        )],
        DataType::TimestampTz,
    );
    let expected = chrono::DateTime::from_timestamp(1_704_187_323, 500_000_000)
        .unwrap()
        .timestamp_millis();

    assert_eq!(
        eval_typed_expr(&expr, &row, &qctx).unwrap(),
        Value::Timestamp(expected)
    );
}

#[test]
fn typed_builtin_age_two_arg_matches_postgres_symbolic_interval() {
    let row = empty_row();
    let qctx = test_qctx();
    let start = chrono::NaiveDate::from_ymd_opt(2024, 3, 15)
        .unwrap()
        .and_hms_milli_opt(12, 34, 56, 789)
        .unwrap()
        .and_utc()
        .timestamp_millis();
    let end = chrono::NaiveDate::from_ymd_opt(2024, 5, 1)
        .unwrap()
        .and_hms_opt(8, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp_millis();
    let expr = func_call(
        "AGE",
        vec![
            const_expr(Value::Timestamp(end), DataType::Timestamp),
            const_expr(Value::Timestamp(start), DataType::Timestamp),
        ],
        DataType::Interval,
    );

    assert_eq!(
        eval_typed_expr(&expr, &row, &qctx).unwrap(),
        Value::Interval(crate::model::IntervalValue::new(
            1,
            16 * 24 * 60 * 60 * 1000 + 19 * 60 * 60 * 1000 + 25 * 60 * 1000 + 3_211,
        ))
    );
}

#[test]
fn typed_builtin_pg_backend_pid_uses_query_context() {
    let row = empty_row();
    let qctx = QueryContext::new(
        99,
        Arc::from("postgres"),
        Arc::from("postgres"),
        1_700_000_000_000,
        1_700_000_000_000,
        Arc::from("UTC"),
    );

    let expr = func_call("PG_BACKEND_PID", vec![], DataType::Int32);
    assert_eq!(
        eval_typed_expr(&expr, &row, &qctx).unwrap(),
        Value::Int32(99)
    );
}

#[test]
fn typed_builtin_pg_backend_pid_truncates_internal_connection_id() {
    let row = empty_row();
    let internal_connection_id = i64::from(i32::MAX) + 77;
    let qctx = QueryContext::new(
        internal_connection_id,
        Arc::from("postgres"),
        Arc::from("postgres"),
        1_700_000_000_000,
        1_700_000_000_000,
        Arc::from("UTC"),
    );

    let expr = func_call("PG_BACKEND_PID", vec![], DataType::Int32);
    assert_eq!(
        eval_typed_expr(&expr, &row, &qctx).unwrap(),
        Value::Int32(internal_connection_id as i32)
    );
}

#[test]
fn typed_builtin_current_database_uses_query_context() {
    let row = empty_row();
    let qctx = QueryContext::new(
        1,
        Arc::from("mydb"),
        Arc::from("postgres"),
        1_700_000_000_000,
        1_700_000_000_000,
        Arc::from("UTC"),
    );

    let expr = func_call("CURRENT_DATABASE", vec![], DataType::Text);
    assert_eq!(
        eval_typed_expr(&expr, &row, &qctx).unwrap(),
        Value::Text("mydb".to_string())
    );
}

#[test]
fn typed_builtin_version_works() {
    let row = empty_row();
    let qctx = test_qctx();

    let expr = func_call("VERSION", vec![], DataType::Text);
    assert_eq!(
        eval_typed_expr(&expr, &row, &qctx).unwrap(),
        Value::Text(crate::sql::expr::VERSION_STRING.to_string())
    );
}

#[test]
fn typed_builtin_unknown_function_errors() {
    let row = empty_row();
    let qctx = test_qctx();

    let expr = func_call("definitely_not_a_real_function", vec![], DataType::Int32);
    let err = eval_typed_expr(&expr, &row, &qctx).unwrap_err();
    assert!(err
        .to_string()
        .contains("unknown function: definitely_not_a_real_function"));
}

#[test]
fn typed_builtin_sha256_returns_bytes() {
    let row = empty_row();
    let qctx = test_qctx();
    let expr = func_call(
        "sha256",
        vec![const_expr(Value::Bytes(b"hello".to_vec()), DataType::Bytes)],
        DataType::Bytes,
    );

    let value = eval_typed_expr(&expr, &row, &qctx).unwrap();
    let Value::Bytes(bytes) = value else {
        panic!("expected bytea result, got {value:?}");
    };
    assert_eq!(
        hex::encode(bytes),
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );
}

#[test]
fn typed_builtin_digest_returns_bytes() {
    let row = empty_row();
    let qctx = test_qctx();
    let expr = func_call(
        "digest",
        vec![
            const_expr(Value::Text("hello".into()), DataType::Text),
            const_expr(Value::Text("sha256".into()), DataType::Text),
        ],
        DataType::Bytes,
    );

    let value = eval_typed_expr(&expr, &row, &qctx).unwrap();
    let Value::Bytes(bytes) = value else {
        panic!("expected bytea result, got {value:?}");
    };
    assert_eq!(
        hex::encode(bytes),
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );
}

#[test]
fn typed_builtin_digest_unknown_algorithm_errors() {
    let row = empty_row();
    let qctx = test_qctx();
    let expr = func_call(
        "digest",
        vec![
            const_expr(Value::Text("abc".into()), DataType::Text),
            const_expr(Value::Text(" sha256 ".into()), DataType::Text),
        ],
        DataType::Bytes,
    );

    let err = eval_typed_expr(&expr, &row, &qctx).unwrap_err();
    let sql_err = err.downcast_ref::<SqlError>().expect("sql error");
    assert_eq!(
        err.to_string(),
        "Cannot use \" sha256 \": No such hash algorithm"
    );
    assert_eq!(sql_err.sqlstate(), "22023");
}

#[test]
fn typed_builtin_digest_evaluates_args_before_null_short_circuit() {
    let row = empty_row();
    let qctx = test_qctx();
    let null_data_expr = func_call(
        "digest",
        vec![
            const_expr(Value::Null, DataType::Text),
            TypedExpr::new(
                TypedExprKind::BinaryOp {
                    left: Box::new(const_expr(Value::Int64(1), DataType::Int64)),
                    op: BinaryOp::Div,
                    right: Box::new(const_expr(Value::Int64(0), DataType::Int64)),
                },
                DataType::Int64,
            ),
        ],
        DataType::Bytes,
    );

    let err = eval_typed_expr(&null_data_expr, &row, &qctx).unwrap_err();
    let sql_err = err.downcast_ref::<SqlError>().expect("sql error");
    assert!(matches!(sql_err, SqlError::DivisionByZero));
    assert_eq!(err.to_string(), "division by zero");

    let null_algorithm_expr = func_call(
        "digest",
        vec![
            func_call(
                "chr",
                vec![const_expr(Value::Int32(-1), DataType::Int32)],
                DataType::Text,
            ),
            const_expr(Value::Null, DataType::Text),
        ],
        DataType::Bytes,
    );

    let err = eval_typed_expr(&null_algorithm_expr, &row, &qctx).unwrap_err();
    let sql_err = err.downcast_ref::<SqlError>().expect("sql error");
    assert!(matches!(sql_err, SqlError::InvalidParameterValue { .. }));
    assert_eq!(err.to_string(), "character number must be positive");
}

#[test]
fn typed_builtin_hashes_follow_pg_overloads() {
    let row = empty_row();
    let qctx = test_qctx();

    let Value::Timestamp(ts) =
        crate::sql::expr::parse_timestamp_string("2024-01-02 03:04:05.678").unwrap()
    else {
        panic!("timestamp literal should parse as Value::Timestamp");
    };

    let md5_expr = func_call(
        "md5",
        vec![const_expr(Value::Text("hello".into()), DataType::Text)],
        DataType::Text,
    );
    assert_eq!(
        eval_typed_expr(&md5_expr, &row, &qctx).unwrap(),
        Value::Text("5d41402abc4b2a76b9719d911017c592".into())
    );

    let md5_timestamp_expr = func_call(
        "md5",
        vec![const_expr(Value::Timestamp(ts), DataType::Timestamp)],
        DataType::Text,
    );
    let md5_timestamp_err = eval_typed_expr(&md5_timestamp_expr, &row, &qctx).unwrap_err();
    let sql_err = md5_timestamp_err
        .downcast_ref::<SqlError>()
        .expect("md5(timestamp) should preserve SQLSTATE");
    assert_eq!(sql_err.sqlstate(), "42883");

    let sha256_expr = func_call(
        "sha256",
        vec![const_expr(Value::Bytes(b"hello".to_vec()), DataType::Bytes)],
        DataType::Bytes,
    );
    let Value::Bytes(sha256_bytes) = eval_typed_expr(&sha256_expr, &row, &qctx).unwrap() else {
        panic!("sha256(bytea) should return bytea");
    };
    assert_eq!(
        hex::encode(sha256_bytes),
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );

    let digest_expr = func_call(
        "digest",
        vec![
            const_expr(Value::Bytes(b"hello".to_vec()), DataType::Bytes),
            const_expr(Value::Text("sha256".into()), DataType::Text),
        ],
        DataType::Bytes,
    );
    let Value::Bytes(digest_bytes) = eval_typed_expr(&digest_expr, &row, &qctx).unwrap() else {
        panic!("digest(bytea, 'sha256') should return bytea");
    };
    assert_eq!(
        hex::encode(digest_bytes),
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );
}

// ── ColumnRef ───────────────────────────────────────────

#[test]
fn test_column_ref() {
    let row = make_row(vec![
        Value::Int64(1),
        Value::Text("foo".into()),
        Value::Boolean(true),
    ]);
    let qctx = test_qctx();

    assert_eq!(
        eval_typed_expr(&col_ref(0, "a", DataType::Int64), &row, &qctx).unwrap(),
        Value::Int64(1)
    );
    assert_eq!(
        eval_typed_expr(&col_ref(1, "b", DataType::Text), &row, &qctx).unwrap(),
        Value::Text("foo".into())
    );
    assert_eq!(
        eval_typed_expr(&col_ref(2, "c", DataType::Boolean), &row, &qctx).unwrap(),
        Value::Boolean(true)
    );
}

#[test]
fn test_column_ref_out_of_bounds() {
    let row = make_row(vec![Value::Int64(1)]);
    let qctx = test_qctx();
    let expr = col_ref(5, "bad", DataType::Int64);
    assert!(eval_typed_expr(&expr, &row, &qctx).is_err());
}

#[test]
fn test_correlated_column_ref_errors() {
    let row = empty_row();
    let qctx = test_qctx();
    let expr = TypedExpr::new(
        TypedExprKind::ColumnRef {
            scope_depth: 1,
            column_index: 0,
            column_name: "outer_col".into(),
        },
        DataType::Int64,
    );
    assert!(eval_typed_expr(&expr, &row, &qctx).is_err());
}

// ── BinaryOp ────────────────────────────────────────────

#[test]
fn test_binary_arithmetic() {
    let row = empty_row();
    let qctx = test_qctx();

    let add = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(const_expr(Value::Int64(10), DataType::Int64)),
            op: BinaryOp::Add,
            right: Box::new(const_expr(Value::Int64(20), DataType::Int64)),
        },
        DataType::Int64,
    );
    assert_eq!(
        eval_typed_expr(&add, &row, &qctx).unwrap(),
        Value::Int64(30)
    );

    let sub = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(const_expr(Value::Int64(30), DataType::Int64)),
            op: BinaryOp::Sub,
            right: Box::new(const_expr(Value::Int64(10), DataType::Int64)),
        },
        DataType::Int64,
    );
    assert_eq!(
        eval_typed_expr(&sub, &row, &qctx).unwrap(),
        Value::Int64(20)
    );
}

#[test]
fn test_binary_comparison() {
    let row = empty_row();
    let qctx = test_qctx();

    let eq = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(const_expr(Value::Int64(5), DataType::Int64)),
            op: BinaryOp::Eq,
            right: Box::new(const_expr(Value::Int64(5), DataType::Int64)),
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&eq, &row, &qctx).unwrap(),
        Value::Boolean(true)
    );

    let lt = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(const_expr(Value::Int64(3), DataType::Int64)),
            op: BinaryOp::Lt,
            right: Box::new(const_expr(Value::Int64(5), DataType::Int64)),
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&lt, &row, &qctx).unwrap(),
        Value::Boolean(true)
    );
}

#[test]
fn test_binary_null_comparison() {
    let row = empty_row();
    let qctx = test_qctx();

    let eq_null = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(const_expr(Value::Int64(5), DataType::Int64)),
            op: BinaryOp::Eq,
            right: Box::new(const_expr(Value::Null, DataType::Int64)),
        },
        DataType::Boolean,
    );
    assert_eq!(eval_typed_expr(&eq_null, &row, &qctx).unwrap(), Value::Null);
}

#[test]
fn test_and_short_circuit() {
    let row = empty_row();
    let qctx = test_qctx();

    // false AND (error) → false (short-circuit)
    let and_false = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(const_expr(Value::Boolean(false), DataType::Boolean)),
            op: BinaryOp::And,
            // This would error if evaluated (column ref on empty row)
            right: Box::new(col_ref(99, "x", DataType::Boolean)),
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&and_false, &row, &qctx).unwrap(),
        Value::Boolean(false)
    );
}

#[test]
fn test_or_short_circuit() {
    let row = empty_row();
    let qctx = test_qctx();

    // true OR (error) → true (short-circuit)
    let or_true = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(const_expr(Value::Boolean(true), DataType::Boolean)),
            op: BinaryOp::Or,
            right: Box::new(col_ref(99, "x", DataType::Boolean)),
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&or_true, &row, &qctx).unwrap(),
        Value::Boolean(true)
    );
}

#[test]
fn test_boolean_pg_fold_prechecks_runtime_guarded_constants() {
    let qctx = test_qctx();

    let runtime_false_and_bad_const = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(col_ref(0, "flag", DataType::Boolean)),
            op: BinaryOp::And,
            right: Box::new(bad_chr_predicate()),
        },
        DataType::Boolean,
    );
    let err = eval_typed_expr(
        &runtime_false_and_bad_const,
        &make_row(vec![Value::Boolean(false)]),
        &qctx,
    )
    .unwrap_err();
    assert_eq!(err.to_string(), "character number must be positive");
    assert_eq!(
        err.downcast_ref::<SqlError>()
            .expect("sql error")
            .sqlstate(),
        "22023"
    );

    let runtime_true_or_bad_const = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(col_ref(0, "flag", DataType::Boolean)),
            op: BinaryOp::Or,
            right: Box::new(bad_chr_predicate()),
        },
        DataType::Boolean,
    );
    let err = eval_typed_expr(
        &runtime_true_or_bad_const,
        &make_row(vec![Value::Boolean(true)]),
        &qctx,
    )
    .unwrap_err();
    assert_eq!(err.to_string(), "character number must be positive");
    assert_eq!(
        err.downcast_ref::<SqlError>()
            .expect("sql error")
            .sqlstate(),
        "22023"
    );
}

#[test]
fn test_boolean_pg_fold_determining_constants_skip_runtime_errors() {
    let row = make_row(vec![Value::Float64(-1.0)]);
    let qctx = test_qctx();

    let sqrt_col_gt_zero = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(builtin_call(
                "sqrt",
                DataType::Float64,
                vec![col_ref(0, "value", DataType::Float64)],
            )),
            op: BinaryOp::Gt,
            right: Box::new(const_expr(Value::Float64(0.0), DataType::Float64)),
        },
        DataType::Boolean,
    );

    let and_false = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(sqrt_col_gt_zero.clone()),
            op: BinaryOp::And,
            right: Box::new(const_expr(Value::Boolean(false), DataType::Boolean)),
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&and_false, &row, &qctx).unwrap(),
        Value::Boolean(false)
    );

    let or_true = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(sqrt_col_gt_zero),
            op: BinaryOp::Or,
            right: Box::new(const_expr(Value::Boolean(true), DataType::Boolean)),
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&or_true, &row, &qctx).unwrap(),
        Value::Boolean(true)
    );
}

#[test]
fn test_string_concat() {
    let row = empty_row();
    let qctx = test_qctx();
    let concat = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(const_expr(Value::Text("hello".into()), DataType::Text)),
            op: BinaryOp::Concat,
            right: Box::new(const_expr(Value::Text(" world".into()), DataType::Text)),
        },
        DataType::Text,
    );
    assert_eq!(
        eval_typed_expr(&concat, &row, &qctx).unwrap(),
        Value::Text("hello world".into())
    );
}

// ── UnaryOp ─────────────────────────────────────────────

#[test]
fn test_unary_not() {
    let row = empty_row();
    let qctx = test_qctx();
    let not = TypedExpr::new(
        TypedExprKind::UnaryOp {
            op: UnaryOp::Not,
            operand: Box::new(const_expr(Value::Boolean(true), DataType::Boolean)),
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&not, &row, &qctx).unwrap(),
        Value::Boolean(false)
    );
}

#[test]
fn test_unary_minus() {
    let row = empty_row();
    let qctx = test_qctx();
    let neg = TypedExpr::new(
        TypedExprKind::UnaryOp {
            op: UnaryOp::Minus,
            operand: Box::new(const_expr(Value::Int64(42), DataType::Int64)),
        },
        DataType::Int64,
    );
    assert_eq!(
        eval_typed_expr(&neg, &row, &qctx).unwrap(),
        Value::Int64(-42)
    );
}

#[test]
fn test_unary_null() {
    let row = empty_row();
    let qctx = test_qctx();
    let neg_null = TypedExpr::new(
        TypedExprKind::UnaryOp {
            op: UnaryOp::Minus,
            operand: Box::new(const_expr(Value::Null, DataType::Int64)),
        },
        DataType::Int64,
    );
    assert_eq!(
        eval_typed_expr(&neg_null, &row, &qctx).unwrap(),
        Value::Null
    );
}

// ── Cast ────────────────────────────────────────────────

#[test]
fn test_cast() {
    use crate::sql::types::CastContext;
    let row = empty_row();
    let qctx = test_qctx();

    let cast_expr = TypedExpr::new(
        TypedExprKind::Cast {
            expr: Box::new(const_expr(Value::Int64(42), DataType::Int64)),
            target_type: DataType::Text,
            cast_context: CastContext::Explicit,
        },
        DataType::Text,
    );
    assert_eq!(
        eval_typed_expr(&cast_expr, &row, &qctx).unwrap(),
        Value::Text("42".into())
    );
}

#[test]
fn test_cast_regtype_to_text_uses_postgres_display_name() {
    let row = empty_row();
    let qctx = test_qctx();

    let cast_expr = TypedExpr::new(
        TypedExprKind::Cast {
            expr: Box::new(const_expr(
                Value::Int64(crate::sql::pg_types::OID_INT4),
                DataType::UserDefined("pg_catalog.regtype".to_string()),
            )),
            target_type: DataType::Text,
            cast_context: CastContext::Explicit,
        },
        DataType::Text,
    );
    assert_eq!(
        eval_typed_expr(&cast_expr, &row, &qctx).unwrap(),
        Value::Text("integer".into())
    );

    let timetz_cast = TypedExpr::new(
        TypedExprKind::Cast {
            expr: Box::new(const_expr(
                Value::Int64(crate::sql::pg_types::OID_TIMETZ),
                DataType::UserDefined("pg_catalog.regtype".to_string()),
            )),
            target_type: DataType::Text,
            cast_context: CastContext::Explicit,
        },
        DataType::Text,
    );
    assert_eq!(
        eval_typed_expr(&timetz_cast, &row, &qctx).unwrap(),
        Value::Text("time with time zone".into())
    );
}

#[test]
fn test_cast_text_to_interval_accepts_fractional_seconds() {
    let row = empty_row();
    let qctx = test_qctx();

    let cast_expr = TypedExpr::new(
        TypedExprKind::Cast {
            expr: Box::new(const_expr(
                Value::Text("5.5 seconds".into()),
                DataType::Text,
            )),
            target_type: DataType::Interval,
            cast_context: CastContext::Explicit,
        },
        DataType::Interval,
    );

    assert_eq!(
        eval_typed_expr(&cast_expr, &row, &qctx).unwrap(),
        Value::Interval(crate::model::IntervalValue::from_millis(5_500))
    );
}

// ── IsTest ──────────────────────────────────────────────

#[test]
fn test_is_null() {
    let row = empty_row();
    let qctx = test_qctx();

    let is_null = TypedExpr::new(
        TypedExprKind::IsTest {
            expr: Box::new(const_expr(Value::Null, DataType::Int64)),
            test: IsTestKind::Null,
            negated: false,
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&is_null, &row, &qctx).unwrap(),
        Value::Boolean(true)
    );

    let is_not_null = TypedExpr::new(
        TypedExprKind::IsTest {
            expr: Box::new(const_expr(Value::Int64(5), DataType::Int64)),
            test: IsTestKind::Null,
            negated: true,
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&is_not_null, &row, &qctx).unwrap(),
        Value::Boolean(true)
    );
}

#[test]
fn test_is_true_false() {
    let row = empty_row();
    let qctx = test_qctx();

    let is_true = TypedExpr::new(
        TypedExprKind::IsTest {
            expr: Box::new(const_expr(Value::Boolean(true), DataType::Boolean)),
            test: IsTestKind::True,
            negated: false,
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&is_true, &row, &qctx).unwrap(),
        Value::Boolean(true)
    );

    let is_false = TypedExpr::new(
        TypedExprKind::IsTest {
            expr: Box::new(const_expr(Value::Boolean(false), DataType::Boolean)),
            test: IsTestKind::False,
            negated: false,
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&is_false, &row, &qctx).unwrap(),
        Value::Boolean(true)
    );
}

// ── Between ─────────────────────────────────────────────

#[test]
fn test_between() {
    let row = empty_row();
    let qctx = test_qctx();
    let bad_high = || {
        builtin_call(
            "sqrt",
            DataType::Float64,
            vec![const_expr(Value::Float64(-1.0), DataType::Float64)],
        )
    };

    let between = TypedExpr::new(
        TypedExprKind::Between {
            expr: Box::new(const_expr(Value::Int64(5), DataType::Int64)),
            low: Box::new(const_expr(Value::Int64(1), DataType::Int64)),
            high: Box::new(const_expr(Value::Int64(10), DataType::Int64)),
            negated: false,
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&between, &row, &qctx).unwrap(),
        Value::Boolean(true)
    );

    let not_between = TypedExpr::new(
        TypedExprKind::Between {
            expr: Box::new(const_expr(Value::Int64(15), DataType::Int64)),
            low: Box::new(const_expr(Value::Int64(1), DataType::Int64)),
            high: Box::new(const_expr(Value::Int64(10), DataType::Int64)),
            negated: false,
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&not_between, &row, &qctx).unwrap(),
        Value::Boolean(false)
    );

    let null_low_false_high = TypedExpr::new(
        TypedExprKind::Between {
            expr: Box::new(const_expr(Value::Int64(5), DataType::Int64)),
            low: Box::new(const_expr(Value::Null, DataType::Int64)),
            high: Box::new(const_expr(Value::Int64(3), DataType::Int64)),
            negated: false,
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&null_low_false_high, &row, &qctx).unwrap(),
        Value::Boolean(false)
    );

    let null_low_true_high = TypedExpr::new(
        TypedExprKind::Between {
            expr: Box::new(const_expr(Value::Int64(5), DataType::Int64)),
            low: Box::new(const_expr(Value::Null, DataType::Int64)),
            high: Box::new(const_expr(Value::Int64(10), DataType::Int64)),
            negated: false,
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&null_low_true_high, &row, &qctx).unwrap(),
        Value::Null
    );

    let short_circuit_false_high_error = TypedExpr::new(
        TypedExprKind::Between {
            expr: Box::new(const_expr(Value::Float64(1.0), DataType::Float64)),
            low: Box::new(const_expr(Value::Float64(10.0), DataType::Float64)),
            high: Box::new(bad_high()),
            negated: false,
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&short_circuit_false_high_error, &row, &qctx).unwrap(),
        Value::Boolean(false)
    );

    let not_between_short_circuit_high_error = TypedExpr::new(
        TypedExprKind::Between {
            expr: Box::new(const_expr(Value::Float64(1.0), DataType::Float64)),
            low: Box::new(const_expr(Value::Float64(10.0), DataType::Float64)),
            high: Box::new(bad_high()),
            negated: true,
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&not_between_short_circuit_high_error, &row, &qctx).unwrap(),
        Value::Boolean(true)
    );

    let true_low_evaluates_high = TypedExpr::new(
        TypedExprKind::Between {
            expr: Box::new(const_expr(Value::Float64(20.0), DataType::Float64)),
            low: Box::new(const_expr(Value::Float64(10.0), DataType::Float64)),
            high: Box::new(bad_high()),
            negated: false,
        },
        DataType::Boolean,
    );
    assert!(eval_typed_expr(&true_low_evaluates_high, &row, &qctx)
        .unwrap_err()
        .to_string()
        .contains("square root"));

    let null_low_evaluates_high = TypedExpr::new(
        TypedExprKind::Between {
            expr: Box::new(const_expr(Value::Float64(1.0), DataType::Float64)),
            low: Box::new(const_expr(Value::Null, DataType::Float64)),
            high: Box::new(bad_high()),
            negated: false,
        },
        DataType::Boolean,
    );
    assert!(eval_typed_expr(&null_low_evaluates_high, &row, &qctx)
        .unwrap_err()
        .to_string()
        .contains("square root"));

    let null_value_evaluates_high = TypedExpr::new(
        TypedExprKind::Between {
            expr: Box::new(const_expr(Value::Null, DataType::Float64)),
            low: Box::new(const_expr(Value::Float64(10.0), DataType::Float64)),
            high: Box::new(bad_high()),
            negated: false,
        },
        DataType::Boolean,
    );
    assert!(eval_typed_expr(&null_value_evaluates_high, &row, &qctx)
        .unwrap_err()
        .to_string()
        .contains("square root"));
}

// ── InList ──────────────────────────────────────────────

#[test]
fn test_in_list() {
    let row = empty_row();
    let qctx = test_qctx();

    let in_list = TypedExpr::new(
        TypedExprKind::InList {
            expr: Box::new(const_expr(Value::Int64(3), DataType::Int64)),
            list: vec![
                const_expr(Value::Int64(1), DataType::Int64),
                const_expr(Value::Int64(2), DataType::Int64),
                const_expr(Value::Int64(3), DataType::Int64),
            ],
            negated: false,
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&in_list, &row, &qctx).unwrap(),
        Value::Boolean(true)
    );

    let not_in_list = TypedExpr::new(
        TypedExprKind::InList {
            expr: Box::new(const_expr(Value::Int64(5), DataType::Int64)),
            list: vec![
                const_expr(Value::Int64(1), DataType::Int64),
                const_expr(Value::Int64(2), DataType::Int64),
            ],
            negated: false,
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&not_in_list, &row, &qctx).unwrap(),
        Value::Boolean(false)
    );
}

#[test]
fn test_in_list_with_null() {
    let row = empty_row();
    let qctx = test_qctx();

    // 5 IN (1, NULL) → NULL (not found, but has NULL)
    let in_null = TypedExpr::new(
        TypedExprKind::InList {
            expr: Box::new(const_expr(Value::Int64(5), DataType::Int64)),
            list: vec![
                const_expr(Value::Int64(1), DataType::Int64),
                const_expr(Value::Null, DataType::Int64),
            ],
            negated: false,
        },
        DataType::Boolean,
    );
    assert_eq!(eval_typed_expr(&in_null, &row, &qctx).unwrap(), Value::Null);
}

#[test]
fn test_in_list_prechecks_foldable_items_before_runtime_short_circuit() {
    let row = make_row(vec![Value::Text("safe".into())]);
    let qctx = test_qctx();

    let in_list = TypedExpr::new(
        TypedExprKind::InList {
            expr: Box::new(col_ref(0, "value", DataType::Text)),
            list: vec![
                const_expr(Value::Text("safe".into()), DataType::Text),
                bad_chr_expr(),
            ],
            negated: false,
        },
        DataType::Boolean,
    );
    let err = eval_typed_expr(&in_list, &row, &qctx).unwrap_err();
    assert_eq!(err.to_string(), "character number must be positive");
    assert_eq!(
        err.downcast_ref::<SqlError>()
            .expect("sql error")
            .sqlstate(),
        "22023"
    );

    let null_lhs = TypedExpr::new(
        TypedExprKind::InList {
            expr: Box::new(col_ref(0, "value", DataType::Text)),
            list: vec![bad_chr_expr()],
            negated: false,
        },
        DataType::Boolean,
    );
    let err = eval_typed_expr(&null_lhs, &make_row(vec![Value::Null]), &qctx).unwrap_err();
    assert_eq!(err.to_string(), "character number must be positive");
}

// ── ScalarArrayCmp ──────────────────────────────────────

#[test]
fn test_scalar_array_cmp_ne_any() {
    let row = empty_row();
    let qctx = test_qctx();

    // 1 <> ANY(ARRAY[1, 2, 3]) → TRUE (1<>2 is true)
    let ne_any = TypedExpr::new(
        TypedExprKind::ScalarArrayCmp {
            expr: Box::new(const_expr(Value::Int64(1), DataType::Int64)),
            elems: vec![
                const_expr(Value::Int64(1), DataType::Int64),
                const_expr(Value::Int64(2), DataType::Int64),
                const_expr(Value::Int64(3), DataType::Int64),
            ],
            op: BinaryOp::NotEq,
            use_or: true,
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&ne_any, &row, &qctx).unwrap(),
        Value::Boolean(true)
    );

    // 1 <> ANY(ARRAY[1, 1, 1]) → FALSE (all equal)
    let ne_any_false = TypedExpr::new(
        TypedExprKind::ScalarArrayCmp {
            expr: Box::new(const_expr(Value::Int64(1), DataType::Int64)),
            elems: vec![
                const_expr(Value::Int64(1), DataType::Int64),
                const_expr(Value::Int64(1), DataType::Int64),
                const_expr(Value::Int64(1), DataType::Int64),
            ],
            op: BinaryOp::NotEq,
            use_or: true,
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&ne_any_false, &row, &qctx).unwrap(),
        Value::Boolean(false)
    );
}

#[test]
fn test_scalar_array_cmp_empty_array() {
    let row = empty_row();
    let qctx = test_qctx();

    // 1 <> ANY(ARRAY[]::int[]) → FALSE (empty array, ANY = FALSE)
    // LHS is still evaluated (important for side-effect correctness)
    let ne_any_empty = TypedExpr::new(
        TypedExprKind::ScalarArrayCmp {
            expr: Box::new(const_expr(Value::Int64(1), DataType::Int64)),
            elems: vec![],
            op: BinaryOp::NotEq,
            use_or: true,
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&ne_any_empty, &row, &qctx).unwrap(),
        Value::Boolean(false)
    );
}

#[test]
fn test_scalar_array_cmp_null_in_array() {
    let row = empty_row();
    let qctx = test_qctx();

    // 1 <> ANY(ARRAY[1, NULL]) → NULL (1<>1 is FALSE, 1<>NULL is NULL → FALSE OR NULL = NULL)
    let ne_any_null = TypedExpr::new(
        TypedExprKind::ScalarArrayCmp {
            expr: Box::new(const_expr(Value::Int64(1), DataType::Int64)),
            elems: vec![
                const_expr(Value::Int64(1), DataType::Int64),
                const_expr(Value::Null, DataType::Int64),
            ],
            op: BinaryOp::NotEq,
            use_or: true,
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&ne_any_null, &row, &qctx).unwrap(),
        Value::Null
    );

    // 1 <> ANY(ARRAY[2, NULL]) → TRUE (1<>2 is TRUE, short-circuit)
    let ne_any_null_true = TypedExpr::new(
        TypedExprKind::ScalarArrayCmp {
            expr: Box::new(const_expr(Value::Int64(1), DataType::Int64)),
            elems: vec![
                const_expr(Value::Int64(2), DataType::Int64),
                const_expr(Value::Null, DataType::Int64),
            ],
            op: BinaryOp::NotEq,
            use_or: true,
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&ne_any_null_true, &row, &qctx).unwrap(),
        Value::Boolean(true)
    );
}

#[test]
fn test_scalar_array_cmp_null_lhs() {
    let row = empty_row();
    let qctx = test_qctx();

    // NULL <> ANY(ARRAY[1, 2]) → NULL
    let ne_any_null_lhs = TypedExpr::new(
        TypedExprKind::ScalarArrayCmp {
            expr: Box::new(const_expr(Value::Null, DataType::Int64)),
            elems: vec![
                const_expr(Value::Int64(1), DataType::Int64),
                const_expr(Value::Int64(2), DataType::Int64),
            ],
            op: BinaryOp::NotEq,
            use_or: true,
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&ne_any_null_lhs, &row, &qctx).unwrap(),
        Value::Null
    );
}

// ── Like ────────────────────────────────────────────────

#[test]
fn test_like() {
    let row = empty_row();
    let qctx = test_qctx();

    let like = TypedExpr::new(
        TypedExprKind::Like {
            expr: Box::new(const_expr(
                Value::Text("hello world".into()),
                DataType::Text,
            )),
            pattern: Box::new(const_expr(Value::Text("hello%".into()), DataType::Text)),
            escape: None,
            case_insensitive: false,
            negated: false,
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&like, &row, &qctx).unwrap(),
        Value::Boolean(true)
    );

    let ilike = TypedExpr::new(
        TypedExprKind::Like {
            expr: Box::new(const_expr(
                Value::Text("Hello World".into()),
                DataType::Text,
            )),
            pattern: Box::new(const_expr(Value::Text("hello%".into()), DataType::Text)),
            escape: None,
            case_insensitive: true,
            negated: false,
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&ilike, &row, &qctx).unwrap(),
        Value::Boolean(true)
    );

    let ilike_with_uppercase_escape = TypedExpr::new(
        TypedExprKind::Like {
            expr: Box::new(const_expr(Value::Text("x_".into()), DataType::Text)),
            pattern: Box::new(const_expr(Value::Text("xA_".into()), DataType::Text)),
            escape: Some(Box::new(const_expr(
                Value::Text("A".into()),
                DataType::Text,
            ))),
            case_insensitive: true,
            negated: false,
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&ilike_with_uppercase_escape, &row, &qctx).unwrap(),
        Value::Boolean(true)
    );

    let like_default_escape = TypedExpr::new(
        TypedExprKind::Like {
            expr: Box::new(const_expr(Value::Text("A_20".into()), DataType::Text)),
            pattern: Box::new(const_expr(Value::Text("A\\_20".into()), DataType::Text)),
            escape: None,
            case_insensitive: false,
            negated: false,
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&like_default_escape, &row, &qctx).unwrap(),
        Value::Boolean(true)
    );

    let ilike_default_escape = TypedExpr::new(
        TypedExprKind::Like {
            expr: Box::new(const_expr(Value::Text("A%Twenty".into()), DataType::Text)),
            pattern: Box::new(const_expr(Value::Text("a\\%twenty".into()), DataType::Text)),
            escape: None,
            case_insensitive: true,
            negated: false,
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&ilike_default_escape, &row, &qctx).unwrap(),
        Value::Boolean(true)
    );

    let like_empty_escape = TypedExpr::new(
        TypedExprKind::Like {
            expr: Box::new(const_expr(Value::Text("A_20".into()), DataType::Text)),
            pattern: Box::new(const_expr(Value::Text("A\\_20".into()), DataType::Text)),
            escape: Some(Box::new(const_expr(
                Value::Text(String::new()),
                DataType::Text,
            ))),
            case_insensitive: false,
            negated: false,
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&like_empty_escape, &row, &qctx).unwrap(),
        Value::Boolean(false)
    );

    let like_multibyte_escape = TypedExpr::new(
        TypedExprKind::Like {
            expr: Box::new(const_expr(Value::Text("A_20".into()), DataType::Text)),
            pattern: Box::new(const_expr(Value::Text("Añ_20".into()), DataType::Text)),
            escape: Some(Box::new(const_expr(
                Value::Text("ñ".into()),
                DataType::Text,
            ))),
            case_insensitive: false,
            negated: false,
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&like_multibyte_escape, &row, &qctx).unwrap(),
        Value::Boolean(true)
    );

    let like_invalid_escape = TypedExpr::new(
        TypedExprKind::Like {
            expr: Box::new(const_expr(Value::Text("abc".into()), DataType::Text)),
            pattern: Box::new(const_expr(Value::Text("abc".into()), DataType::Text)),
            escape: Some(Box::new(const_expr(
                Value::Text("xx".into()),
                DataType::Text,
            ))),
            case_insensitive: false,
            negated: false,
        },
        DataType::Boolean,
    );
    let err = eval_typed_expr(&like_invalid_escape, &row, &qctx).unwrap_err();
    let sql_err = err.downcast_ref::<SqlError>().expect("sql error");
    assert!(matches!(sql_err, SqlError::InvalidEscapeString { .. }));
    assert_eq!(sql_err.sqlstate(), "22025");
    assert_eq!(err.to_string(), "invalid escape string");

    let pattern_null = TypedExpr::new(
        TypedExprKind::Like {
            expr: Box::new(const_expr(Value::Text("abc".into()), DataType::Text)),
            pattern: Box::new(const_expr(Value::Null, DataType::Text)),
            escape: Some(Box::new(const_expr(
                Value::Text("xx".into()),
                DataType::Text,
            ))),
            case_insensitive: false,
            negated: false,
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&pattern_null, &row, &qctx).unwrap(),
        Value::Null
    );
}

#[test]
fn test_ilike_does_not_expand_unicode_characters() {
    let row = empty_row();
    let qctx = test_qctx();

    let unicode_single_char = TypedExpr::new(
        TypedExprKind::Like {
            expr: Box::new(const_expr(Value::Text("\u{0130}".into()), DataType::Text)),
            pattern: Box::new(const_expr(Value::Text("_".into()), DataType::Text)),
            escape: None,
            case_insensitive: true,
            negated: false,
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&unicode_single_char, &row, &qctx).unwrap(),
        Value::Boolean(true)
    );

    let unicode_two_char_pattern = TypedExpr::new(
        TypedExprKind::Like {
            expr: Box::new(const_expr(Value::Text("\u{0130}".into()), DataType::Text)),
            pattern: Box::new(const_expr(Value::Text("i_".into()), DataType::Text)),
            escape: None,
            case_insensitive: true,
            negated: false,
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&unicode_two_char_pattern, &row, &qctx).unwrap(),
        Value::Boolean(false)
    );

    let unicode_simple_match = TypedExpr::new(
        TypedExprKind::Like {
            expr: Box::new(const_expr(Value::Text("\u{0130}".into()), DataType::Text)),
            pattern: Box::new(const_expr(Value::Text("i".into()), DataType::Text)),
            escape: None,
            case_insensitive: true,
            negated: false,
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&unicode_simple_match, &row, &qctx).unwrap(),
        Value::Boolean(true)
    );

    let unicode_does_not_expand_to_two_chars = TypedExpr::new(
        TypedExprKind::Like {
            expr: Box::new(const_expr(Value::Text("\u{00DF}".into()), DataType::Text)),
            pattern: Box::new(const_expr(Value::Text("ss".into()), DataType::Text)),
            escape: None,
            case_insensitive: true,
            negated: false,
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&unicode_does_not_expand_to_two_chars, &row, &qctx).unwrap(),
        Value::Boolean(false)
    );

    let unicode_sigma_match = TypedExpr::new(
        TypedExprKind::Like {
            expr: Box::new(const_expr(Value::Text("\u{03A3}".into()), DataType::Text)),
            pattern: Box::new(const_expr(Value::Text("\u{03C3}".into()), DataType::Text)),
            escape: None,
            case_insensitive: true,
            negated: false,
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&unicode_sigma_match, &row, &qctx).unwrap(),
        Value::Boolean(true)
    );
}

#[test]
fn test_like_null_escape_returns_null() {
    let row = empty_row();
    let qctx = test_qctx();

    for (case_insensitive, negated) in [(false, false), (true, false), (false, true), (true, true)]
    {
        let expr = TypedExpr::new(
            TypedExprKind::Like {
                expr: Box::new(const_expr(Value::Text("abc".into()), DataType::Text)),
                pattern: Box::new(const_expr(Value::Text("abc".into()), DataType::Text)),
                escape: Some(Box::new(const_expr(Value::Null, DataType::Text))),
                case_insensitive,
                negated,
            },
            DataType::Boolean,
        );
        assert_eq!(eval_typed_expr(&expr, &row, &qctx).unwrap(), Value::Null);
    }
}

#[test]
fn test_like_trailing_escape_after_consumed_input_returns_false() {
    let row = empty_row();
    let qctx = test_qctx();
    let expr = TypedExpr::new(
        TypedExprKind::Like {
            expr: Box::new(const_expr(Value::Text("a".into()), DataType::Text)),
            pattern: Box::new(const_expr(Value::Text("a\\".into()), DataType::Text)),
            escape: Some(Box::new(const_expr(
                Value::Text("\\".into()),
                DataType::Text,
            ))),
            case_insensitive: false,
            negated: false,
        },
        DataType::Boolean,
    );

    assert_eq!(
        eval_typed_expr(&expr, &row, &qctx).unwrap(),
        Value::Boolean(false)
    );
}

#[test]
fn test_like_trailing_escape_errors_when_matching_unconsumed_input() {
    let row = empty_row();
    let qctx = test_qctx();
    let expr = TypedExpr::new(
        TypedExprKind::Like {
            expr: Box::new(const_expr(Value::Text("a\\".into()), DataType::Text)),
            pattern: Box::new(const_expr(Value::Text("a\\".into()), DataType::Text)),
            escape: Some(Box::new(const_expr(
                Value::Text("\\".into()),
                DataType::Text,
            ))),
            case_insensitive: false,
            negated: false,
        },
        DataType::Boolean,
    );

    let err = eval_typed_expr(&expr, &row, &qctx).unwrap_err();
    assert!(
        err.to_string()
            .contains("LIKE pattern must not end with escape character"),
        "unexpected error: {err}"
    );
    let sql_err = err.downcast_ref::<SqlError>().expect("sql error");
    assert!(matches!(sql_err, SqlError::InvalidEscapeString { .. }));
    assert_eq!(sql_err.sqlstate(), "22025");
}

// ── Case ────────────────────────────────────────────────

#[test]
fn test_searched_case() {
    let row = empty_row();
    let qctx = test_qctx();

    let case = TypedExpr::new(
        TypedExprKind::Case {
            operand: None,
            when_clauses: vec![
                (
                    const_expr(Value::Boolean(false), DataType::Boolean),
                    const_expr(Value::Text("no".into()), DataType::Text),
                ),
                (
                    const_expr(Value::Boolean(true), DataType::Boolean),
                    const_expr(Value::Text("yes".into()), DataType::Text),
                ),
            ],
            else_result: Some(Box::new(const_expr(
                Value::Text("else".into()),
                DataType::Text,
            ))),
        },
        DataType::Text,
    );
    assert_eq!(
        eval_typed_expr(&case, &row, &qctx).unwrap(),
        Value::Text("yes".into())
    );
}

#[test]
fn test_simple_case() {
    let row = empty_row();
    let qctx = test_qctx();

    let case = TypedExpr::new(
        TypedExprKind::Case {
            operand: Some(Box::new(const_expr(Value::Int64(2), DataType::Int64))),
            when_clauses: vec![
                (
                    const_expr(Value::Int64(1), DataType::Int64),
                    const_expr(Value::Text("one".into()), DataType::Text),
                ),
                (
                    const_expr(Value::Int64(2), DataType::Int64),
                    const_expr(Value::Text("two".into()), DataType::Text),
                ),
            ],
            else_result: None,
        },
        DataType::Text,
    );
    assert_eq!(
        eval_typed_expr(&case, &row, &qctx).unwrap(),
        Value::Text("two".into())
    );
}

// ── Coalesce ────────────────────────────────────────────

#[test]
fn test_coalesce() {
    let row = empty_row();
    let qctx = test_qctx();

    let coalesce = TypedExpr::new(
        TypedExprKind::Coalesce(vec![
            const_expr(Value::Null, DataType::Int64),
            const_expr(Value::Null, DataType::Int64),
            const_expr(Value::Int64(42), DataType::Int64),
            const_expr(Value::Int64(99), DataType::Int64),
        ]),
        DataType::Int64,
    );
    assert_eq!(
        eval_typed_expr(&coalesce, &row, &qctx).unwrap(),
        Value::Int64(42)
    );
}

#[test]
fn test_coalesce_prechecks_fold_candidates_after_runtime_arg() {
    let row = make_row(vec![Value::Text("safe".into())]);
    let qctx = test_qctx();

    let constant_guard = TypedExpr::new(
        TypedExprKind::Coalesce(vec![
            const_expr(Value::Text("safe".into()), DataType::Text),
            builtin_call(
                "chr",
                DataType::Text,
                vec![const_expr(Value::Int32(-1), DataType::Int32)],
            ),
        ]),
        DataType::Text,
    );
    assert_eq!(
        eval_typed_expr(&constant_guard, &row, &qctx).unwrap(),
        Value::Text("safe".into())
    );

    let runtime_guard = TypedExpr::new(
        TypedExprKind::Coalesce(vec![
            col_ref(0, "nickname", DataType::Text),
            builtin_call(
                "chr",
                DataType::Text,
                vec![const_expr(Value::Int32(-1), DataType::Int32)],
            ),
        ]),
        DataType::Text,
    );
    let err = eval_typed_expr(&runtime_guard, &row, &qctx).unwrap_err();
    let sql_err = err.downcast_ref::<SqlError>().expect("sql error");
    assert!(matches!(sql_err, SqlError::InvalidParameterValue { .. }));
    assert_eq!(sql_err.sqlstate(), "22023");

    let nested_fold_error = TypedExpr::new(
        TypedExprKind::Coalesce(vec![
            col_ref(0, "nickname", DataType::Text),
            builtin_call(
                "left",
                DataType::Text,
                vec![
                    col_ref(0, "nickname", DataType::Text),
                    builtin_call(
                        "abs",
                        DataType::Int32,
                        vec![const_expr(Value::Int32(i32::MIN), DataType::Int32)],
                    ),
                ],
            ),
        ]),
        DataType::Text,
    );
    let err = eval_typed_expr(&nested_fold_error, &row, &qctx).unwrap_err();
    let sql_err = err.downcast_ref::<SqlError>().expect("sql error");
    assert!(matches!(sql_err, SqlError::NumericValueOutOfRange { .. }));
    assert_eq!(sql_err.sqlstate(), "22003");
}

// ── NullIf ──────────────────────────────────────────────

#[test]
fn test_nullif() {
    let row = empty_row();
    let qctx = test_qctx();

    // NULLIF(5, 5) → NULL
    let nullif_eq = TypedExpr::new(
        TypedExprKind::NullIf(
            Box::new(const_expr(Value::Int64(5), DataType::Int64)),
            Box::new(const_expr(Value::Int64(5), DataType::Int64)),
        ),
        DataType::Int64,
    );
    assert_eq!(
        eval_typed_expr(&nullif_eq, &row, &qctx).unwrap(),
        Value::Null
    );

    // NULLIF(5, 3) → 5
    let nullif_ne = TypedExpr::new(
        TypedExprKind::NullIf(
            Box::new(const_expr(Value::Int64(5), DataType::Int64)),
            Box::new(const_expr(Value::Int64(3), DataType::Int64)),
        ),
        DataType::Int64,
    );
    assert_eq!(
        eval_typed_expr(&nullif_ne, &row, &qctx).unwrap(),
        Value::Int64(5)
    );
}

// ── MinMax ──────────────────────────────────────────────

#[test]
fn test_greatest_least() {
    let row = empty_row();
    let qctx = test_qctx();

    let greatest = TypedExpr::new(
        TypedExprKind::MinMax {
            args: vec![
                const_expr(Value::Int64(3), DataType::Int64),
                const_expr(Value::Int64(7), DataType::Int64),
                const_expr(Value::Int64(1), DataType::Int64),
            ],
            is_greatest: true,
        },
        DataType::Int64,
    );
    assert_eq!(
        eval_typed_expr(&greatest, &row, &qctx).unwrap(),
        Value::Int64(7)
    );

    let least = TypedExpr::new(
        TypedExprKind::MinMax {
            args: vec![
                const_expr(Value::Int64(3), DataType::Int64),
                const_expr(Value::Int64(7), DataType::Int64),
                const_expr(Value::Int64(1), DataType::Int64),
            ],
            is_greatest: false,
        },
        DataType::Int64,
    );
    assert_eq!(
        eval_typed_expr(&least, &row, &qctx).unwrap(),
        Value::Int64(1)
    );
}

// ── FunctionCall ────────────────────────────────────────

#[test]
fn test_function_call_abs() {
    let row = empty_row();
    let qctx = test_qctx();

    let abs = TypedExpr::new(
        TypedExprKind::FunctionCall {
            func: ResolvedFunction {
                name: "abs".into(),
                kind: FunctionKind::Builtin,
                return_type: DataType::Float64,
            },
            args: vec![const_expr(Value::Float64(-42.0), DataType::Float64)],
            order_by: vec![],
            filter: None,
        },
        DataType::Float64,
    );
    assert_eq!(
        eval_typed_expr(&abs, &row, &qctx).unwrap(),
        Value::Float64(42.0)
    );
}

#[test]
fn test_function_call_pg_typeof_uses_typed_argument_type() {
    let row = empty_row();
    let qctx = test_qctx();

    let pg_typeof = TypedExpr::new(
        TypedExprKind::FunctionCall {
            func: ResolvedFunction {
                name: "pg_typeof".into(),
                kind: FunctionKind::Builtin,
                return_type: DataType::UserDefined("pg_catalog.regtype".to_string()),
            },
            args: vec![const_expr(
                Value::Int64(1259),
                DataType::UserDefined("pg_catalog.regclass".to_string()),
            )],
            order_by: vec![],
            filter: None,
        },
        DataType::UserDefined("pg_catalog.regtype".to_string()),
    );
    assert_eq!(
        eval_typed_expr(&pg_typeof, &row, &qctx).unwrap(),
        Value::Text("regclass".into())
    );
}

// Regression: pg_typeof(column) must return the declared column type even when
// the runtime value is NULL (#1507).
#[test]
fn test_pg_typeof_null_column_returns_declared_type() {
    // Row where column 0 is NULL at runtime.
    let row = make_row(vec![Value::Null]);
    let qctx = test_qctx();

    let pg_typeof = TypedExpr::new(
        TypedExprKind::FunctionCall {
            func: ResolvedFunction {
                name: "PG_TYPEOF".into(),
                kind: FunctionKind::Builtin,
                return_type: DataType::UserDefined("pg_catalog.regtype".to_string()),
            },
            args: vec![col_ref(0, "valuntil", DataType::TimestampTz)],
            order_by: vec![],
            filter: None,
        },
        DataType::UserDefined("pg_catalog.regtype".to_string()),
    );
    assert_eq!(
        eval_typed_expr(&pg_typeof, &row, &qctx).unwrap(),
        Value::Text("timestamp with time zone".into())
    );
}

#[test]
fn test_json_functions_use_typed_timestamptz_timezone() {
    let row = empty_row();
    let qctx = QueryContext::new(
        1,
        Arc::from("postgres"),
        Arc::from("postgres"),
        1_700_000_000_000,
        1_700_000_000_000,
        Arc::from("Asia/Shanghai"),
    );
    let ts = const_expr(Value::Timestamp(1_699_992_800_000), DataType::TimestampTz);

    assert_eq!(
        eval_typed_expr(
            &builtin_call("TO_JSON", DataType::Json, vec![ts.clone()]),
            &row,
            &qctx,
        )
        .unwrap(),
        Value::Json("\"2023-11-15T04:13:20+08:00\"".into())
    );
    assert_eq!(
        eval_typed_expr(
            &builtin_call("TO_JSONB", DataType::Jsonb, vec![ts.clone()]),
            &row,
            &qctx,
        )
        .unwrap(),
        Value::Jsonb("\"2023-11-15T04:13:20+08:00\"".into())
    );
    assert_eq!(
        eval_typed_expr(
            &builtin_call("JSON_BUILD_ARRAY", DataType::Json, vec![ts.clone()]),
            &row,
            &qctx,
        )
        .unwrap(),
        Value::Json("[\"2023-11-15T04:13:20+08:00\"]".into())
    );

    let row_arg = TypedExpr::new(
        TypedExprKind::Row(vec![ts]),
        DataType::UserDefined("record".into()),
    );
    assert_eq!(
        eval_typed_expr(
            &builtin_call("TO_JSON", DataType::Json, vec![row_arg.clone()]),
            &row,
            &qctx,
        )
        .unwrap(),
        Value::Json("{\"f1\":\"2023-11-15T04:13:20+08:00\"}".into())
    );
    assert_eq!(
        eval_typed_expr(
            &builtin_call("ROW_TO_JSON", DataType::Json, vec![row_arg]),
            &row,
            &qctx,
        )
        .unwrap(),
        Value::Json("{\"f1\":\"2023-11-15T04:13:20+08:00\"}".into())
    );
}

// ── ArrayLiteral ────────────────────────────────────────

#[test]
fn test_array_literal() {
    let row = empty_row();
    let qctx = test_qctx();

    let arr = TypedExpr::new(
        TypedExprKind::ArrayLiteral(vec![
            const_expr(Value::Int64(1), DataType::Int64),
            const_expr(Value::Int64(2), DataType::Int64),
            const_expr(Value::Int64(3), DataType::Int64),
        ]),
        DataType::Array(Box::new(DataType::Int64)),
    );
    assert_eq!(
        eval_typed_expr(&arr, &row, &qctx).unwrap(),
        Value::Array(vec![Value::Int64(1), Value::Int64(2), Value::Int64(3)])
    );
}

// ── ArrayIndex ──────────────────────────────────────────

#[test]
fn test_array_index() {
    let row = empty_row();
    let qctx = test_qctx();

    // arr[2] (1-based) → second element
    let idx = TypedExpr::new(
        TypedExprKind::ArrayIndex {
            array: Box::new(TypedExpr::new(
                TypedExprKind::ArrayLiteral(vec![
                    const_expr(Value::Int64(10), DataType::Int64),
                    const_expr(Value::Int64(20), DataType::Int64),
                    const_expr(Value::Int64(30), DataType::Int64),
                ]),
                DataType::Array(Box::new(DataType::Int64)),
            )),
            index: Box::new(const_expr(Value::Int64(2), DataType::Int64)),
        },
        DataType::Int64,
    );
    assert_eq!(
        eval_typed_expr(&idx, &row, &qctx).unwrap(),
        Value::Int64(20)
    );

    // Out of bounds → NULL
    let oob = TypedExpr::new(
        TypedExprKind::ArrayIndex {
            array: Box::new(TypedExpr::new(
                TypedExprKind::ArrayLiteral(vec![const_expr(Value::Int64(10), DataType::Int64)]),
                DataType::Array(Box::new(DataType::Int64)),
            )),
            index: Box::new(const_expr(Value::Int64(5), DataType::Int64)),
        },
        DataType::Int64,
    );
    assert_eq!(eval_typed_expr(&oob, &row, &qctx).unwrap(), Value::Null);
}

// ── Row ─────────────────────────────────────────────────

#[test]
fn test_row_constructor() {
    let row = empty_row();
    let qctx = test_qctx();

    let row_expr = TypedExpr::new(
        TypedExprKind::Row(vec![
            const_expr(Value::Int64(1), DataType::Int64),
            const_expr(Value::Text("a".into()), DataType::Text),
        ]),
        DataType::Boolean, // DataType doesn't matter for Row
    );
    assert_eq!(
        eval_typed_expr(&row_expr, &row, &qctx).unwrap(),
        Value::Array(vec![Value::Int64(1), Value::Text("a".into())])
    );
}

// ── Aggregate / Window / Subquery errors ────────────────

#[test]
fn test_aggregate_errors() {
    let row = empty_row();
    let qctx = test_qctx();
    let agg = TypedExpr::new(
        TypedExprKind::AggregateCall {
            func: ResolvedFunction {
                name: "count".into(),
                kind: FunctionKind::Builtin,
                return_type: DataType::Int64,
            },
            args: vec![],
            distinct: false,
            order_by: vec![],
            filter: None,
        },
        DataType::Int64,
    );
    assert!(eval_typed_expr(&agg, &row, &qctx).is_err());
}

#[test]
fn test_subquery_errors() {
    let row = empty_row();
    let qctx = test_qctx();
    let exists = TypedExpr::new(
        TypedExprKind::Exists {
            subquery: Box::new(AnalyzedQuery {
                ctes: vec![],
                body: AnalyzedQueryBody::Select(AnalyzedSelect {
                    projection: vec![],
                    from: vec![],
                    where_clause: None,
                    group_by: vec![],
                    having: None,
                    distinct: AnalyzedDistinct::All,
                }),
                order_by: vec![],
                limit: None,
                offset: None,
                output_schema: vec![],
            }),
            negated: false,
        },
        DataType::Boolean,
    );
    assert!(eval_typed_expr(&exists, &row, &qctx).is_err());
}

// ── Column-based evaluation ─────────────────────────────

#[test]
fn test_column_based_filter() {
    // Simulate: SELECT * FROM t WHERE a > 10
    let row = make_row(vec![Value::Int64(15), Value::Text("hello".into())]);
    let qctx = test_qctx();

    let filter = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(col_ref(0, "a", DataType::Int64)),
            op: BinaryOp::Gt,
            right: Box::new(const_expr(Value::Int64(10), DataType::Int64)),
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&filter, &row, &qctx).unwrap(),
        Value::Boolean(true)
    );
}

#[test]
fn test_complex_expression() {
    // Simulate: CASE WHEN a > 10 THEN 'big' WHEN a > 5 THEN 'medium' ELSE 'small' END
    let row = make_row(vec![Value::Int64(7)]);
    let qctx = test_qctx();

    let case = TypedExpr::new(
        TypedExprKind::Case {
            operand: None,
            when_clauses: vec![
                (
                    TypedExpr::new(
                        TypedExprKind::BinaryOp {
                            left: Box::new(col_ref(0, "a", DataType::Int64)),
                            op: BinaryOp::Gt,
                            right: Box::new(const_expr(Value::Int64(10), DataType::Int64)),
                        },
                        DataType::Boolean,
                    ),
                    const_expr(Value::Text("big".into()), DataType::Text),
                ),
                (
                    TypedExpr::new(
                        TypedExprKind::BinaryOp {
                            left: Box::new(col_ref(0, "a", DataType::Int64)),
                            op: BinaryOp::Gt,
                            right: Box::new(const_expr(Value::Int64(5), DataType::Int64)),
                        },
                        DataType::Boolean,
                    ),
                    const_expr(Value::Text("medium".into()), DataType::Text),
                ),
            ],
            else_result: Some(Box::new(const_expr(
                Value::Text("small".into()),
                DataType::Text,
            ))),
        },
        DataType::Text,
    );
    assert_eq!(
        eval_typed_expr(&case, &row, &qctx).unwrap(),
        Value::Text("medium".into())
    );
}

#[test]
fn test_exp_operator() {
    let row = empty_row();
    let qctx = test_qctx();

    let exp = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(const_expr(Value::Float64(2.0), DataType::Float64)),
            op: BinaryOp::Exp,
            right: Box::new(const_expr(Value::Float64(3.0), DataType::Float64)),
        },
        DataType::Float64,
    );
    assert_eq!(
        eval_typed_expr(&exp, &row, &qctx).unwrap(),
        Value::Float64(8.0)
    );
}

#[test]
fn test_bitwise_and() {
    let row = empty_row();
    let qctx = test_qctx();

    // Int64 & Int64 → Int64
    let bw_and = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(const_expr(Value::Int64(0b1100), DataType::Int64)),
            op: BinaryOp::BitwiseAnd,
            right: Box::new(const_expr(Value::Int64(0b1010), DataType::Int64)),
        },
        DataType::Int64,
    );
    assert_eq!(
        eval_typed_expr(&bw_and, &row, &qctx).unwrap(),
        Value::Int64(0b1000)
    );
}

#[test]
fn test_bitwise_xor() {
    let row = empty_row();
    let qctx = test_qctx();

    let bw_xor = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(const_expr(Value::Int32(5), DataType::Int32)),
            op: BinaryOp::BitwiseXor,
            right: Box::new(const_expr(Value::Int32(3), DataType::Int32)),
        },
        DataType::Int32,
    );
    assert_eq!(
        eval_typed_expr(&bw_xor, &row, &qctx).unwrap(),
        Value::Int32(6)
    );
}

#[test]
fn test_bitwise_not() {
    let row = empty_row();
    let qctx = test_qctx();

    let bw_not = TypedExpr::new(
        TypedExprKind::UnaryOp {
            op: UnaryOp::BitwiseNot,
            operand: Box::new(const_expr(Value::Int64(0), DataType::Int64)),
        },
        DataType::Int64,
    );
    assert_eq!(
        eval_typed_expr(&bw_not, &row, &qctx).unwrap(),
        Value::Int64(-1) // !0 = -1 in two's complement
    );
}

// ── Shift safety ───────────────────────────────────────

#[test]
fn test_shift_left_basic() {
    let row = empty_row();
    let qctx = test_qctx();
    let shl = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(const_expr(Value::Int64(1), DataType::Int64)),
            op: BinaryOp::ShiftLeft,
            right: Box::new(const_expr(Value::Int32(3), DataType::Int32)),
        },
        DataType::Int64,
    );
    assert_eq!(eval_typed_expr(&shl, &row, &qctx).unwrap(), Value::Int64(8));
}

#[test]
fn test_shift_right_basic() {
    let row = empty_row();
    let qctx = test_qctx();
    let shr = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(const_expr(Value::Int64(16), DataType::Int64)),
            op: BinaryOp::ShiftRight,
            right: Box::new(const_expr(Value::Int32(2), DataType::Int32)),
        },
        DataType::Int64,
    );
    assert_eq!(eval_typed_expr(&shr, &row, &qctx).unwrap(), Value::Int64(4));
}

#[test]
fn test_shift_large_amounts_use_masked_counts_like_pg() {
    let row = empty_row();
    let qctx = test_qctx();

    let shl_64 = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(const_expr(Value::Int64(1), DataType::Int64)),
            op: BinaryOp::ShiftLeft,
            right: Box::new(const_expr(Value::Int64(64), DataType::Int64)),
        },
        DataType::Int64,
    );
    assert_eq!(
        eval_typed_expr(&shl_64, &row, &qctx).unwrap(),
        Value::Int64(1)
    );

    let shl_32 = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(const_expr(Value::Int32(1), DataType::Int32)),
            op: BinaryOp::ShiftLeft,
            right: Box::new(const_expr(Value::Int32(32), DataType::Int32)),
        },
        DataType::Int32,
    );
    assert_eq!(
        eval_typed_expr(&shl_32, &row, &qctx).unwrap(),
        Value::Int32(1)
    );

    let shl_33 = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(const_expr(Value::Int32(1), DataType::Int32)),
            op: BinaryOp::ShiftLeft,
            right: Box::new(const_expr(Value::Int32(33), DataType::Int32)),
        },
        DataType::Int32,
    );
    assert_eq!(
        eval_typed_expr(&shl_33, &row, &qctx).unwrap(),
        Value::Int32(2)
    );

    let shr_65 = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(const_expr(Value::Int64(2), DataType::Int64)),
            op: BinaryOp::ShiftRight,
            right: Box::new(const_expr(Value::Int32(65), DataType::Int32)),
        },
        DataType::Int64,
    );
    assert_eq!(
        eval_typed_expr(&shr_65, &row, &qctx).unwrap(),
        Value::Int64(1)
    );
}

#[test]
fn test_shift_negative_amounts_use_masked_counts_like_pg() {
    let row = empty_row();
    let qctx = test_qctx();
    let shl_neg = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(const_expr(Value::Int64(1), DataType::Int64)),
            op: BinaryOp::ShiftLeft,
            right: Box::new(const_expr(Value::Int32(-1), DataType::Int32)),
        },
        DataType::Int64,
    );
    assert_eq!(
        eval_typed_expr(&shl_neg, &row, &qctx).unwrap(),
        Value::Int64(1_i64 << 63)
    );

    let shr_neg = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(const_expr(Value::Int32(1), DataType::Int32)),
            op: BinaryOp::ShiftRight,
            right: Box::new(const_expr(Value::Int32(-1), DataType::Int32)),
        },
        DataType::Int32,
    );
    assert_eq!(
        eval_typed_expr(&shr_neg, &row, &qctx).unwrap(),
        Value::Int32(0)
    );
}

#[test]
fn test_shift_upper_bound_ok() {
    let row = empty_row();
    let qctx = test_qctx();

    // int4: 1 << 31
    let shl_31 = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(const_expr(Value::Int32(1), DataType::Int32)),
            op: BinaryOp::ShiftLeft,
            right: Box::new(const_expr(Value::Int32(31), DataType::Int32)),
        },
        DataType::Int32,
    );
    assert_eq!(
        eval_typed_expr(&shl_31, &row, &qctx).unwrap(),
        Value::Int32(1_i32 << 31)
    );

    // int8: 1 << 63
    let shl_63 = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(const_expr(Value::Int64(1), DataType::Int64)),
            op: BinaryOp::ShiftLeft,
            right: Box::new(const_expr(Value::Int32(63), DataType::Int32)),
        },
        DataType::Int64,
    );
    assert_eq!(
        eval_typed_expr(&shl_63, &row, &qctx).unwrap(),
        Value::Int64(1_i64 << 63)
    );
}

#[test]
fn test_shift_null_propagation() {
    let row = empty_row();
    let qctx = test_qctx();
    let shl_null = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(const_expr(Value::Int64(1), DataType::Int64)),
            op: BinaryOp::ShiftLeft,
            right: Box::new(const_expr(Value::Null, DataType::Int32)),
        },
        DataType::Int64,
    );
    assert_eq!(
        eval_typed_expr(&shl_null, &row, &qctx).unwrap(),
        Value::Null
    );
}

// ── Bitwise type preservation ──────────────────────────

#[test]
fn test_bitwise_preserves_int32() {
    let row = empty_row();
    let qctx = test_qctx();

    // Int32 & Int32 → Int32
    let bw = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(const_expr(Value::Int32(0xFF), DataType::Int32)),
            op: BinaryOp::BitwiseAnd,
            right: Box::new(const_expr(Value::Int32(0x0F), DataType::Int32)),
        },
        DataType::Int32,
    );
    assert_eq!(
        eval_typed_expr(&bw, &row, &qctx).unwrap(),
        Value::Int32(0x0F)
    );
}

#[test]
fn test_shift_preserves_int32() {
    let row = empty_row();
    let qctx = test_qctx();

    // Int32 << Int32 → Int32
    let shl = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(const_expr(Value::Int32(1), DataType::Int32)),
            op: BinaryOp::ShiftLeft,
            right: Box::new(const_expr(Value::Int32(3), DataType::Int32)),
        },
        DataType::Int32,
    );
    assert_eq!(eval_typed_expr(&shl, &row, &qctx).unwrap(), Value::Int32(8));
}

#[test]
fn test_bitwise_int32_int64_promotes_to_int64() {
    let row = empty_row();
    let qctx = test_qctx();

    // Int32 & Int64 → Int64
    let bw = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(const_expr(Value::Int32(0xFF), DataType::Int32)),
            op: BinaryOp::BitwiseAnd,
            right: Box::new(const_expr(Value::Int64(0x0F), DataType::Int64)),
        },
        DataType::Int64,
    );
    assert_eq!(
        eval_typed_expr(&bw, &row, &qctx).unwrap(),
        Value::Int64(0x0F)
    );
}

// ── Context-dependent functions ────────────────────────

#[test]
fn test_current_schema() {
    let row = empty_row();
    let qctx = test_qctx();
    let func = TypedExpr::new(
        TypedExprKind::FunctionCall {
            func: ResolvedFunction {
                name: "current_schema".into(),
                kind: FunctionKind::Builtin,
                return_type: DataType::Text,
            },
            args: vec![],
            order_by: vec![],
            filter: None,
        },
        DataType::Text,
    );
    assert_eq!(
        eval_typed_expr(&func, &row, &qctx).unwrap(),
        Value::Text("public".into())
    );
}

#[test]
fn test_current_user() {
    let row = empty_row();
    let qctx = test_qctx();
    let func = TypedExpr::new(
        TypedExprKind::FunctionCall {
            func: ResolvedFunction {
                name: "current_user".into(),
                kind: FunctionKind::Builtin,
                return_type: DataType::Text,
            },
            args: vec![],
            order_by: vec![],
            filter: None,
        },
        DataType::Text,
    );
    assert_eq!(
        eval_typed_expr(&func, &row, &qctx).unwrap(),
        Value::Text("postgres".into())
    );
}

#[test]
fn typed_builtin_current_setting_masks_embedding_api_key_from_explicit_qctx_snapshot() {
    let row = empty_row();
    let mut qctx = test_qctx();
    let mut snapshot = HashMap::new();
    snapshot.insert(
        "embedding.api_key".to_string(),
        "sk-secret-1234".to_string(),
    );
    qctx.settings_snapshot = Some(Arc::new(snapshot));

    let expr = func_call(
        "CURRENT_SETTING",
        vec![const_expr(
            Value::Text("embedding.api_key".to_string()),
            DataType::Text,
        )],
        DataType::Text,
    );

    assert_eq!(
        eval_typed_expr(&expr, &row, &qctx).unwrap(),
        Value::Text("****".to_string())
    );
}

#[test]
fn typed_builtin_current_setting_missing_ok_returns_null_from_explicit_qctx_snapshot() {
    let row = empty_row();
    let mut qctx = test_qctx();
    qctx.settings_snapshot = Some(Arc::new(HashMap::new()));

    let expr = func_call(
        "CURRENT_SETTING",
        vec![
            const_expr(Value::Text("missing.setting".to_string()), DataType::Text),
            const_expr(Value::Boolean(true), DataType::Boolean),
        ],
        DataType::Text,
    );

    assert_eq!(eval_typed_expr(&expr, &row, &qctx).unwrap(), Value::Null);
}

#[test]
fn typed_builtin_current_setting_missing_ok_still_errors_without_snapshot() {
    let row = empty_row();
    let qctx = test_qctx();
    let expr = func_call(
        "CURRENT_SETTING",
        vec![
            const_expr(Value::Text("missing.setting".to_string()), DataType::Text),
            const_expr(Value::Boolean(true), DataType::Boolean),
        ],
        DataType::Text,
    );

    let err = eval_typed_expr(&expr, &row, &qctx).unwrap_err();
    assert!(err
        .to_string()
        .contains("unrecognized configuration parameter \"missing.setting\""));
}

#[test]
fn typed_builtin_current_setting_rejected_public_guc_still_errors_with_missing_ok() {
    let row = empty_row();
    let mut qctx = test_qctx();
    qctx.settings_snapshot = Some(Arc::new(HashMap::new()));

    let expr = func_call(
        "CURRENT_SETTING",
        vec![
            const_expr(
                Value::Text("db9.enable_cop_agg_pushdown".to_string()),
                DataType::Text,
            ),
            const_expr(Value::Boolean(true), DataType::Boolean),
        ],
        DataType::Text,
    );

    let err = eval_typed_expr(&expr, &row, &qctx).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("unrecognized configuration parameter \"db9.enable_cop_agg_pushdown\""));
    assert!(msg.contains("db9.enable_cop_pushdown"));
}

#[test]
fn typed_builtin_set_config_null_reset_rejected_public_guc_errors() {
    let row = empty_row();
    let mut qctx = test_qctx();
    qctx.settings_snapshot = Some(Arc::new(HashMap::new()));

    let expr = func_call(
        "SET_CONFIG",
        vec![
            const_expr(
                Value::Text("db9.enable_cop_agg_pushdown".to_string()),
                DataType::Text,
            ),
            const_expr(Value::Null, DataType::Text),
            const_expr(Value::Boolean(false), DataType::Boolean),
        ],
        DataType::Text,
    );

    let err = eval_typed_expr(&expr, &row, &qctx).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("unrecognized configuration parameter \"db9.enable_cop_agg_pushdown\""));
    assert!(msg.contains("db9.enable_cop_pushdown"));
}

#[test]
fn test_auth_uid_returns_null_when_unset() {
    let row = empty_row();
    let qctx = test_qctx();
    let func = TypedExpr::new(
        TypedExprKind::FunctionCall {
            func: ResolvedFunction {
                name: "AUTH.UID".into(),
                kind: FunctionKind::Builtin,
                return_type: DataType::Text,
            },
            args: vec![],
            order_by: vec![],
            filter: None,
        },
        DataType::Text,
    );
    assert_eq!(eval_typed_expr(&func, &row, &qctx).unwrap(), Value::Null);
}

#[test]
fn test_auth_uid_rejects_arguments() {
    let row = empty_row();
    let qctx = test_qctx();
    let func = TypedExpr::new(
        TypedExprKind::FunctionCall {
            func: ResolvedFunction {
                name: "AUTH.UID".into(),
                kind: FunctionKind::Builtin,
                return_type: DataType::Text,
            },
            args: vec![const_expr(Value::Int32(1), DataType::Int32)],
            order_by: vec![],
            filter: None,
        },
        DataType::Text,
    );
    let err = eval_typed_expr(&func, &row, &qctx).unwrap_err();
    assert!(
        err.to_string().contains("auth.uid"),
        "expected function-not-found error, got: {}",
        err
    );
}

#[test]
fn test_version() {
    let row = empty_row();
    let qctx = test_qctx();
    let func = TypedExpr::new(
        TypedExprKind::FunctionCall {
            func: ResolvedFunction {
                name: "version".into(),
                kind: FunctionKind::Builtin,
                return_type: DataType::Text,
            },
            args: vec![],
            order_by: vec![],
            filter: None,
        },
        DataType::Text,
    );
    let result = eval_typed_expr(&func, &row, &qctx).unwrap();
    match result {
        Value::Text(s) => assert!(s.contains("db9-server")),
        _ => panic!("expected text"),
    }
}

#[test]
fn test_sequence_function_errors() {
    let row = empty_row();
    let qctx = test_qctx();
    let func = TypedExpr::new(
        TypedExprKind::FunctionCall {
            func: ResolvedFunction {
                name: "nextval".into(),
                kind: FunctionKind::Builtin,
                return_type: DataType::Int64,
            },
            args: vec![const_expr(Value::Text("seq1".into()), DataType::Text)],
            order_by: vec![],
            filter: None,
        },
        DataType::Int64,
    );
    assert!(eval_typed_expr(&func, &row, &qctx).is_err());
}

// ── Context-dependent timestamp functions ────────────────

#[test]
fn test_now_uses_explicit_qctx() {
    let row = empty_row();
    let qctx = QueryContext::new(
        1,
        Arc::from("postgres"),
        Arc::from("postgres"),
        1_700_000_000_000,
        1_700_000_000_000,
        Arc::from("UTC"),
    );
    let func = TypedExpr::new(
        TypedExprKind::FunctionCall {
            func: ResolvedFunction {
                name: "now".into(),
                kind: FunctionKind::Builtin,
                return_type: DataType::Timestamp,
            },
            args: vec![],
            order_by: vec![],
            filter: None,
        },
        DataType::Timestamp,
    );
    let result = eval_typed_expr(&func, &row, &qctx).unwrap();
    assert!(matches!(result, Value::Timestamp(_)));
}

#[test]
fn test_pg_backend_pid_uses_explicit_qctx() {
    let row = empty_row();
    let qctx = QueryContext::new(
        42,
        Arc::from("postgres"),
        Arc::from("postgres"),
        1_700_000_000_000,
        1_700_000_000_000,
        Arc::from("UTC"),
    );
    let func = TypedExpr::new(
        TypedExprKind::FunctionCall {
            func: ResolvedFunction {
                name: "pg_backend_pid".into(),
                kind: FunctionKind::Builtin,
                return_type: DataType::Int32,
            },
            args: vec![],
            order_by: vec![],
            filter: None,
        },
        DataType::Int32,
    );
    assert_eq!(
        eval_typed_expr(&func, &row, &qctx).unwrap(),
        Value::Int32(42)
    );
}

#[test]
fn test_current_database_uses_explicit_qctx() {
    let row = empty_row();
    let qctx = QueryContext::new(
        1,
        Arc::from("mydb"),
        Arc::from("postgres"),
        1_700_000_000_000,
        1_700_000_000_000,
        Arc::from("UTC"),
    );
    let func = TypedExpr::new(
        TypedExprKind::FunctionCall {
            func: ResolvedFunction {
                name: "current_database".into(),
                kind: FunctionKind::Builtin,
                return_type: DataType::Text,
            },
            args: vec![],
            order_by: vec![],
            filter: None,
        },
        DataType::Text,
    );
    assert_eq!(
        eval_typed_expr(&func, &row, &qctx).unwrap(),
        Value::Text("mydb".into())
    );
}

// ── Interval arithmetic ───────────────────────────────

#[test]
fn test_interval_add_timestamp() {
    let row = empty_row();
    let qctx = test_qctx();
    // Timestamp + Interval(1 hour) → Timestamp
    let expr = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(const_expr(Value::Timestamp(1_000_000), DataType::Timestamp)),
            op: BinaryOp::Add,
            right: Box::new(const_expr(
                Value::Interval(crate::model::IntervalValue::new(0, 3_600_000)),
                DataType::Interval,
            )),
        },
        DataType::Timestamp,
    );
    assert_eq!(
        eval_typed_expr(&expr, &row, &qctx).unwrap(),
        Value::Timestamp(1_000_000 + 3_600_000)
    );
}

#[test]
fn test_interval_sub_timestamp() {
    let row = empty_row();
    let qctx = test_qctx();
    // Timestamp - Interval(1 hour) → Timestamp
    let expr = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(const_expr(
                Value::Timestamp(10_000_000),
                DataType::Timestamp,
            )),
            op: BinaryOp::Sub,
            right: Box::new(const_expr(
                Value::Interval(crate::model::IntervalValue::new(0, 3_600_000)),
                DataType::Interval,
            )),
        },
        DataType::Timestamp,
    );
    assert_eq!(
        eval_typed_expr(&expr, &row, &qctx).unwrap(),
        Value::Timestamp(10_000_000 - 3_600_000)
    );
}

#[test]
fn test_interval_add_intervals() {
    let row = empty_row();
    let qctx = test_qctx();
    // Interval(1h) + Interval(2h) → Interval(3h)
    let expr = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(const_expr(
                Value::Interval(crate::model::IntervalValue::new(0, 3_600_000)),
                DataType::Interval,
            )),
            op: BinaryOp::Add,
            right: Box::new(const_expr(
                Value::Interval(crate::model::IntervalValue::new(0, 7_200_000)),
                DataType::Interval,
            )),
        },
        DataType::Interval,
    );
    assert_eq!(
        eval_typed_expr(&expr, &row, &qctx).unwrap(),
        Value::Interval(crate::model::IntervalValue::new(0, 10_800_000))
    );
}

#[test]
fn test_timestamp_diff() {
    let row = empty_row();
    let qctx = test_qctx();
    // Timestamp - Timestamp → Interval
    let expr = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(const_expr(
                Value::Timestamp(10_000_000),
                DataType::Timestamp,
            )),
            op: BinaryOp::Sub,
            right: Box::new(const_expr(Value::Timestamp(3_000_000), DataType::Timestamp)),
        },
        DataType::Interval,
    );
    assert_eq!(
        eval_typed_expr(&expr, &row, &qctx).unwrap(),
        Value::Interval(crate::model::IntervalValue::from_millis(7_000_000))
    );
}

#[test]
fn test_date_add_interval() {
    let row = empty_row();
    let qctx = test_qctx();
    // Date + Interval → Timestamp
    // days since epoch 0 = 1970-01-01 → ts=0; interval 1 hour = 3_600_000ms
    let expr = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(const_expr(Value::Date(0), DataType::Date)),
            op: BinaryOp::Add,
            right: Box::new(const_expr(
                Value::Interval(crate::model::IntervalValue::new(0, 3_600_000)),
                DataType::Interval,
            )),
        },
        DataType::Timestamp,
    );
    let result = eval_typed_expr(&expr, &row, &qctx).unwrap();
    assert!(matches!(result, Value::Timestamp(_)));
}

// ── String functions ──────────────────────────────────

fn func_call(name: &str, args: Vec<TypedExpr>, ret: DataType) -> TypedExpr {
    TypedExpr::new(
        TypedExprKind::FunctionCall {
            func: ResolvedFunction {
                name: name.into(),
                kind: FunctionKind::Builtin,
                return_type: ret.clone(),
            },
            args,
            order_by: vec![],
            filter: None,
        },
        ret,
    )
}

#[test]
fn test_trim_function() {
    let row = empty_row();
    let qctx = test_qctx();
    let expr = func_call(
        "btrim",
        vec![const_expr(Value::Text("  hello  ".into()), DataType::Text)],
        DataType::Text,
    );
    assert_eq!(
        eval_typed_expr(&expr, &row, &qctx).unwrap(),
        Value::Text("hello".into())
    );
}

#[test]
fn test_ltrim_rtrim() {
    let row = empty_row();
    let qctx = test_qctx();
    let ltrim = func_call(
        "ltrim",
        vec![const_expr(Value::Text("  hello".into()), DataType::Text)],
        DataType::Text,
    );
    assert_eq!(
        eval_typed_expr(&ltrim, &row, &qctx).unwrap(),
        Value::Text("hello".into())
    );

    let rtrim = func_call(
        "rtrim",
        vec![const_expr(Value::Text("hello  ".into()), DataType::Text)],
        DataType::Text,
    );
    assert_eq!(
        eval_typed_expr(&rtrim, &row, &qctx).unwrap(),
        Value::Text("hello".into())
    );
}

#[test]
fn test_position_function() {
    let row = empty_row();
    let qctx = test_qctx();
    // POSITION('lo' IN 'hello') → 4 (1-based)
    let expr = func_call(
        "strpos",
        vec![
            const_expr(Value::Text("hello".into()), DataType::Text),
            const_expr(Value::Text("lo".into()), DataType::Text),
        ],
        DataType::Int32,
    );
    assert_eq!(
        eval_typed_expr(&expr, &row, &qctx).unwrap(),
        Value::Int32(4)
    );
}

#[test]
fn test_array_position_start_argument() {
    let row = empty_row();
    let qctx = test_qctx();
    let array_type = DataType::Array(Box::new(DataType::Text));
    let expr = func_call(
        "array_position",
        vec![
            const_expr(
                Value::Array(vec![
                    Value::Text("a".into()),
                    Value::Text("b".into()),
                    Value::Text("b".into()),
                ]),
                array_type,
            ),
            const_expr(Value::Text("b".into()), DataType::Text),
            const_expr(Value::Int32(3), DataType::Int32),
        ],
        DataType::Int32,
    );
    assert_eq!(
        eval_typed_expr(&expr, &row, &qctx).unwrap(),
        Value::Int32(3)
    );

    let null_start = func_call(
        "array_position",
        vec![
            const_expr(
                Value::Array(vec![Value::Text("a".into())]),
                DataType::Array(Box::new(DataType::Text)),
            ),
            const_expr(Value::Text("a".into()), DataType::Text),
            const_expr(Value::Null, DataType::Int32),
        ],
        DataType::Int32,
    );
    assert!(eval_typed_expr(&null_start, &row, &qctx)
        .unwrap_err()
        .to_string()
        .contains("initial position must not be null"));
}

#[test]
fn test_array_position_remove_append_prepend_multidimensional_errors() {
    let row = empty_row();
    let qctx = test_qctx();
    let array_type = DataType::Array(Box::new(DataType::Array(Box::new(DataType::Int32))));

    let multidimensional = const_expr(
        Value::Array(vec![
            Value::Array(vec![Value::Int32(1), Value::Int32(2)]),
            Value::Array(vec![Value::Int32(3), Value::Int32(4)]),
        ]),
        array_type.clone(),
    );

    let position = func_call(
        "array_position",
        vec![
            multidimensional.clone(),
            const_expr(Value::Int32(1), DataType::Int32),
        ],
        DataType::Int32,
    );
    let err = eval_typed_expr(&position, &row, &qctx).unwrap_err();
    assert_eq!(
        err.downcast_ref::<SqlError>()
            .expect("sql error")
            .sqlstate(),
        "0A000"
    );
    assert_eq!(
        err.to_string(),
        "searching for elements in multidimensional arrays is not supported"
    );

    let remove = func_call(
        "array_remove",
        vec![
            multidimensional.clone(),
            const_expr(Value::Int32(1), DataType::Int32),
        ],
        array_type.clone(),
    );
    let err = eval_typed_expr(&remove, &row, &qctx).unwrap_err();
    assert_eq!(
        err.downcast_ref::<SqlError>()
            .expect("sql error")
            .sqlstate(),
        "0A000"
    );
    assert_eq!(
        err.to_string(),
        "removing elements from multidimensional arrays is not supported"
    );

    let append = func_call(
        "array_append",
        vec![
            multidimensional.clone(),
            const_expr(Value::Int32(5), DataType::Int32),
        ],
        array_type.clone(),
    );
    let err = eval_typed_expr(&append, &row, &qctx).unwrap_err();
    assert_eq!(
        err.downcast_ref::<SqlError>()
            .expect("sql error")
            .sqlstate(),
        "22000"
    );
    assert_eq!(
        err.to_string(),
        "argument must be empty or one-dimensional array"
    );

    let prepend = func_call(
        "array_prepend",
        vec![
            const_expr(Value::Int32(5), DataType::Int32),
            multidimensional,
        ],
        array_type,
    );
    let err = eval_typed_expr(&prepend, &row, &qctx).unwrap_err();
    assert_eq!(
        err.downcast_ref::<SqlError>()
            .expect("sql error")
            .sqlstate(),
        "22000"
    );
    assert_eq!(
        err.to_string(),
        "argument must be empty or one-dimensional array"
    );
}

#[test]
fn test_array_cat_mixed_rank_runtime_matches_postgres() {
    let row = empty_row();
    let qctx = test_qctx();
    let array_type_1d = DataType::Array(Box::new(DataType::Int32));
    let array_type_2d = DataType::Array(Box::new(array_type_1d.clone()));

    let one_d = const_expr(
        Value::Array(vec![Value::Int32(1), Value::Int32(2)]),
        array_type_1d.clone(),
    );
    let two_d = const_expr(
        Value::Array(vec![
            Value::Array(vec![Value::Int32(3), Value::Int32(4)]),
            Value::Array(vec![Value::Int32(5), Value::Int32(6)]),
        ]),
        array_type_2d.clone(),
    );

    let left_result = func_call(
        "array_cat",
        vec![one_d.clone(), two_d.clone()],
        array_type_2d.clone(),
    );
    assert_eq!(
        eval_typed_expr(&left_result, &row, &qctx).unwrap(),
        Value::Array(vec![
            Value::Array(vec![Value::Int32(1), Value::Int32(2)]),
            Value::Array(vec![Value::Int32(3), Value::Int32(4)]),
            Value::Array(vec![Value::Int32(5), Value::Int32(6)]),
        ])
    );

    let right_result = func_call("array_cat", vec![two_d, one_d], array_type_2d);
    assert_eq!(
        eval_typed_expr(&right_result, &row, &qctx).unwrap(),
        Value::Array(vec![
            Value::Array(vec![Value::Int32(3), Value::Int32(4)]),
            Value::Array(vec![Value::Int32(5), Value::Int32(6)]),
            Value::Array(vec![Value::Int32(1), Value::Int32(2)]),
        ])
    );
}

#[test]
fn test_substring_function() {
    let row = empty_row();
    let qctx = test_qctx();
    // SUBSTRING('hello' FROM 2 FOR 3) → 'ell'
    let expr = func_call(
        "substring",
        vec![
            const_expr(Value::Text("hello".into()), DataType::Text),
            const_expr(Value::Int64(2), DataType::Int64),
            const_expr(Value::Int64(3), DataType::Int64),
        ],
        DataType::Text,
    );
    assert_eq!(
        eval_typed_expr(&expr, &row, &qctx).unwrap(),
        Value::Text("ell".into())
    );
}

#[test]
fn test_upper_lower() {
    let row = empty_row();
    let qctx = test_qctx();
    let upper = func_call(
        "upper",
        vec![const_expr(Value::Text("hello".into()), DataType::Text)],
        DataType::Text,
    );
    assert_eq!(
        eval_typed_expr(&upper, &row, &qctx).unwrap(),
        Value::Text("HELLO".into())
    );

    let lower = func_call(
        "lower",
        vec![const_expr(Value::Text("HELLO".into()), DataType::Text)],
        DataType::Text,
    );
    assert_eq!(
        eval_typed_expr(&lower, &row, &qctx).unwrap(),
        Value::Text("hello".into())
    );
}

#[test]
fn test_replace_function() {
    let row = empty_row();
    let qctx = test_qctx();
    let expr = func_call(
        "replace",
        vec![
            const_expr(Value::Text("abc".into()), DataType::Text),
            const_expr(Value::Text("b".into()), DataType::Text),
            const_expr(Value::Text("x".into()), DataType::Text),
        ],
        DataType::Text,
    );
    assert_eq!(
        eval_typed_expr(&expr, &row, &qctx).unwrap(),
        Value::Text("axc".into())
    );
}

// ── UUID/Bytea ────────────────────────────────────────

#[test]
fn test_uuid_generation() {
    let row = empty_row();
    let qctx = test_qctx();
    let expr = func_call("gen_random_uuid", vec![], DataType::Uuid);
    let result = eval_typed_expr(&expr, &row, &qctx).unwrap();
    assert!(matches!(result, Value::Uuid(_)));
}

#[test]
fn test_cast_text_to_uuid() {
    let row = empty_row();
    let qctx = test_qctx();
    let expr = TypedExpr::new(
        TypedExprKind::Cast {
            expr: Box::new(const_expr(
                Value::Text("550e8400-e29b-41d4-a716-446655440000".into()),
                DataType::Text,
            )),
            target_type: DataType::Uuid,
            cast_context: CastContext::Explicit,
        },
        DataType::Uuid,
    );
    let result = eval_typed_expr(&expr, &row, &qctx).unwrap();
    assert!(matches!(result, Value::Uuid(_)));
}

#[test]
fn test_bytea_length() {
    let row = empty_row();
    let qctx = test_qctx();
    let expr = func_call(
        "octet_length",
        vec![const_expr(
            Value::Bytes(vec![1, 2, 3, 4, 5]),
            DataType::Bytes,
        )],
        DataType::Int32,
    );
    assert_eq!(
        eval_typed_expr(&expr, &row, &qctx).unwrap(),
        Value::Int32(5)
    );
}

// ── Division by zero / NaN ────────────────────────────

#[test]
fn test_int_division_by_zero() {
    let row = empty_row();
    let qctx = test_qctx();
    let expr = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(const_expr(Value::Int64(1), DataType::Int64)),
            op: BinaryOp::Div,
            right: Box::new(const_expr(Value::Int64(0), DataType::Int64)),
        },
        DataType::Int64,
    );
    let err = eval_typed_expr(&expr, &row, &qctx).unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("division by zero"),
        "expected 'division by zero' error, got: {}",
        err
    );
}

#[test]
fn test_float_division_by_zero() {
    let row = empty_row();
    let qctx = test_qctx();
    let expr = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(const_expr(Value::Float64(1.0), DataType::Float64)),
            op: BinaryOp::Div,
            right: Box::new(const_expr(Value::Float64(0.0), DataType::Float64)),
        },
        DataType::Float64,
    );
    // PostgreSQL: float8 / 0 → Infinity (not an error)
    let result = eval_typed_expr(&expr, &row, &qctx);
    match result {
        Ok(Value::Float64(f)) => assert!(f.is_infinite(), "expected Infinity, got {}", f),
        Err(e) => {
            // Also acceptable if implementation errors on division by zero
            assert!(e.to_string().to_lowercase().contains("division by zero"));
        }
        other => panic!("unexpected result: {:?}", other),
    }
}

#[test]
fn test_numeric_division_by_zero() {
    use rust_decimal::Decimal;
    let row = empty_row();
    let qctx = test_qctx();
    let numeric_dt = DataType::Numeric {
        precision: None,
        scale: None,
    };
    let expr = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(const_expr(
                Value::Numeric(Decimal::new(1, 0)),
                numeric_dt.clone(),
            )),
            op: BinaryOp::Div,
            right: Box::new(const_expr(
                Value::Numeric(Decimal::new(0, 0)),
                numeric_dt.clone(),
            )),
        },
        numeric_dt,
    );
    let err = eval_typed_expr(&expr, &row, &qctx).unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("division by zero"),
        "expected 'division by zero' error, got: {}",
        err
    );
}

#[test]
fn test_modulo_by_zero() {
    let row = empty_row();
    let qctx = test_qctx();
    let expr = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(const_expr(Value::Int64(5), DataType::Int64)),
            op: BinaryOp::Mod,
            right: Box::new(const_expr(Value::Int64(0), DataType::Int64)),
        },
        DataType::Int64,
    );
    let err = eval_typed_expr(&expr, &row, &qctx).unwrap_err();
    assert!(matches!(
        err.downcast_ref::<SqlError>(),
        Some(SqlError::DivisionByZero)
    ));
    assert_eq!(err.to_string(), "division by zero");
}

// ── AT TIME ZONE ──────────────────────────────────────

#[test]
fn test_at_time_zone_utc() {
    let row = empty_row();
    let qctx = test_qctx();
    // TIMESTAMP AT TIME ZONE 'UTC' → no offset change
    let expr = func_call(
        "timezone",
        vec![
            const_expr(Value::Text("UTC".into()), DataType::Text),
            TypedExpr::new(
                TypedExprKind::Constant(Value::Timestamp(1_700_000_000_000)),
                DataType::Timestamp,
            ),
        ],
        DataType::Timestamp,
    );
    assert_eq!(
        eval_typed_expr(&expr, &row, &qctx).unwrap(),
        Value::Timestamp(1_700_000_000_000)
    );
}

#[test]
fn test_at_time_zone_named() {
    let row = empty_row();
    let qctx = test_qctx();
    // TIMESTAMPTZ AT TIME ZONE 'America/New_York' → converts from UTC to EST/EDT
    let expr = TypedExpr::new(
        TypedExprKind::FunctionCall {
            func: ResolvedFunction {
                name: "timezone".into(),
                kind: FunctionKind::Builtin,
                return_type: DataType::Timestamp,
            },
            args: vec![
                const_expr(Value::Text("America/New_York".into()), DataType::Text),
                TypedExpr::new(
                    TypedExprKind::Constant(Value::Timestamp(1_700_000_000_000)),
                    DataType::TimestampTz,
                ),
            ],
            order_by: vec![],
            filter: None,
        },
        DataType::Timestamp,
    );
    let result = eval_typed_expr(&expr, &row, &qctx).unwrap();
    // Should shift by EST offset (-5h = -18_000_000ms)
    assert!(matches!(result, Value::Timestamp(_)));
    if let Value::Timestamp(ms) = result {
        assert_ne!(ms, 1_700_000_000_000, "should have applied timezone offset");
    }
}

#[test]
fn test_at_time_zone_offset() {
    let row = empty_row();
    let qctx = test_qctx();
    // PostgreSQL uses POSIX sign convention for numeric offsets:
    // '+08:00' means UTC-8, so TIMESTAMP AT TIME ZONE '+08:00' adds 8h.
    let expr = TypedExpr::new(
        TypedExprKind::FunctionCall {
            func: ResolvedFunction {
                name: "timezone".into(),
                kind: FunctionKind::Builtin,
                return_type: DataType::Timestamp,
            },
            args: vec![
                const_expr(Value::Text("+08:00".into()), DataType::Text),
                TypedExpr::new(
                    TypedExprKind::Constant(Value::Timestamp(1_700_000_000_000)),
                    DataType::Timestamp,
                ),
            ],
            order_by: vec![],
            filter: None,
        },
        DataType::Timestamp,
    );
    let result = eval_typed_expr(&expr, &row, &qctx).unwrap();
    assert_eq!(result, Value::Timestamp(1_700_000_000_000 + 8 * 3_600_000));
}

// ── Cast edge cases ───────────────────────────────────

#[test]
fn test_cast_float_nan() {
    let row = empty_row();
    let qctx = test_qctx();
    let expr = TypedExpr::new(
        TypedExprKind::Cast {
            expr: Box::new(const_expr(Value::Text("NaN".into()), DataType::Text)),
            target_type: DataType::Float64,
            cast_context: CastContext::Explicit,
        },
        DataType::Float64,
    );
    let result = eval_typed_expr(&expr, &row, &qctx).unwrap();
    match result {
        Value::Float64(f) => assert!(f.is_nan(), "expected NaN, got {}", f),
        other => panic!("expected Float64, got {:?}", other),
    }
}

#[test]
fn test_cast_float_infinity() {
    let row = empty_row();
    let qctx = test_qctx();
    let expr = TypedExpr::new(
        TypedExprKind::Cast {
            expr: Box::new(const_expr(Value::Text("Infinity".into()), DataType::Text)),
            target_type: DataType::Float64,
            cast_context: CastContext::Explicit,
        },
        DataType::Float64,
    );
    let result = eval_typed_expr(&expr, &row, &qctx).unwrap();
    match result {
        Value::Float64(f) => {
            assert!(f.is_infinite() && f > 0.0, "expected Infinity, got {}", f)
        }
        other => panic!("expected Float64, got {:?}", other),
    }
}

#[test]
fn test_cast_interval_from_string() {
    let row = empty_row();
    let qctx = test_qctx();
    let expr = TypedExpr::new(
        TypedExprKind::Cast {
            expr: Box::new(const_expr(
                Value::Text("1 day 2 hours".into()),
                DataType::Text,
            )),
            target_type: DataType::Interval,
            cast_context: CastContext::Explicit,
        },
        DataType::Interval,
    );
    let result = eval_typed_expr(&expr, &row, &qctx).unwrap();
    match result {
        Value::Interval(iv) => {
            // 1 day = 86_400_000ms, 2 hours = 7_200_000ms
            let expected_ms = 86_400_000 + 7_200_000;
            assert_eq!(iv.millis, expected_ms);
        }
        other => panic!("expected Interval, got {:?}", other),
    }
}

// ── Misc evaluator paths ──────────────────────────────

#[test]
fn test_similar_to() {
    let row = empty_row();
    let qctx = test_qctx();
    // 'hello' SIMILAR TO 'h%o' → true
    let expr = TypedExpr::new(
        TypedExprKind::SimilarTo {
            expr: Box::new(const_expr(Value::Text("hello".into()), DataType::Text)),
            pattern: Box::new(const_expr(Value::Text("h%o".into()), DataType::Text)),
            escape: None,
            negated: false,
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&expr, &row, &qctx).unwrap(),
        Value::Boolean(true)
    );

    // 'hello' NOT SIMILAR TO 'x%' → true
    let not_similar = TypedExpr::new(
        TypedExprKind::SimilarTo {
            expr: Box::new(const_expr(Value::Text("hello".into()), DataType::Text)),
            pattern: Box::new(const_expr(Value::Text("x%".into()), DataType::Text)),
            escape: None,
            negated: true,
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&not_similar, &row, &qctx).unwrap(),
        Value::Boolean(true)
    );

    let similar_empty_escape = TypedExpr::new(
        TypedExprKind::SimilarTo {
            expr: Box::new(const_expr(Value::Text("a_c".into()), DataType::Text)),
            pattern: Box::new(const_expr(Value::Text("a\\_c".into()), DataType::Text)),
            escape: Some(Box::new(const_expr(
                Value::Text(String::new()),
                DataType::Text,
            ))),
            negated: false,
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&similar_empty_escape, &row, &qctx).unwrap(),
        Value::Boolean(false)
    );

    let similar_invalid_pattern = TypedExpr::new(
        TypedExprKind::SimilarTo {
            expr: Box::new(const_expr(Value::Text("abc".into()), DataType::Text)),
            pattern: Box::new(const_expr(Value::Text("[".into()), DataType::Text)),
            escape: None,
            negated: false,
        },
        DataType::Boolean,
    );
    let err = eval_typed_expr(&similar_invalid_pattern, &row, &qctx).unwrap_err();
    assert_eq!(
        err.to_string(),
        "invalid regular expression: brackets [] not balanced"
    );
    assert_eq!(
        err.downcast_ref::<SqlError>()
            .expect("sql error")
            .sqlstate(),
        "2201B"
    );
}

#[test]
fn test_like_null_source_invalid_escape_errors_before_null_short_circuit() {
    let row = empty_row();
    let qctx = test_qctx();

    let expr = TypedExpr::new(
        TypedExprKind::Like {
            expr: Box::new(const_expr(Value::Null, DataType::Text)),
            pattern: Box::new(const_expr(Value::Text("abc".into()), DataType::Text)),
            escape: Some(Box::new(const_expr(
                Value::Text("xx".into()),
                DataType::Text,
            ))),
            case_insensitive: false,
            negated: false,
        },
        DataType::Boolean,
    );

    let err = eval_typed_expr(&expr, &row, &qctx).unwrap_err();
    let sql_err = err.downcast_ref::<SqlError>().expect("sql error");
    assert!(matches!(sql_err, SqlError::InvalidEscapeString { .. }));
    assert_eq!(sql_err.sqlstate(), "22025");
    assert_eq!(err.to_string(), "invalid escape string");

    let pattern_null = TypedExpr::new(
        TypedExprKind::SimilarTo {
            expr: Box::new(const_expr(Value::Text("abc".into()), DataType::Text)),
            pattern: Box::new(const_expr(Value::Null, DataType::Text)),
            escape: Some(Box::new(const_expr(
                Value::Text("xx".into()),
                DataType::Text,
            ))),
            negated: false,
        },
        DataType::Boolean,
    );
    assert_eq!(
        eval_typed_expr(&pattern_null, &row, &qctx).unwrap(),
        Value::Null
    );
}

#[test]
fn test_similar_to_null_source_invalid_escape_errors_before_null_short_circuit() {
    let row = empty_row();
    let qctx = test_qctx();

    let expr = TypedExpr::new(
        TypedExprKind::SimilarTo {
            expr: Box::new(const_expr(Value::Null, DataType::Text)),
            pattern: Box::new(const_expr(Value::Text("abc".into()), DataType::Text)),
            escape: Some(Box::new(const_expr(
                Value::Text("xx".into()),
                DataType::Text,
            ))),
            negated: false,
        },
        DataType::Boolean,
    );

    let err = eval_typed_expr(&expr, &row, &qctx).unwrap_err();
    let sql_err = err.downcast_ref::<SqlError>().expect("sql error");
    assert!(matches!(sql_err, SqlError::InvalidEscapeString { .. }));
    assert_eq!(sql_err.sqlstate(), "22025");
    assert_eq!(err.to_string(), "invalid escape string");
}

#[test]
fn test_similar_to_null_escape_returns_null() {
    let row = empty_row();
    let qctx = test_qctx();

    for negated in [false, true] {
        let expr = TypedExpr::new(
            TypedExprKind::SimilarTo {
                expr: Box::new(const_expr(Value::Text("abc".into()), DataType::Text)),
                pattern: Box::new(const_expr(Value::Text("abc".into()), DataType::Text)),
                escape: Some(Box::new(const_expr(Value::Null, DataType::Text))),
                negated,
            },
            DataType::Boolean,
        );
        assert_eq!(eval_typed_expr(&expr, &row, &qctx).unwrap(), Value::Null);
    }
}

#[test]
fn test_array_literal_nested() {
    let row = empty_row();
    let qctx = test_qctx();
    // ARRAY[1, 2, 3]
    let expr = TypedExpr::new(
        TypedExprKind::ArrayLiteral(vec![
            const_expr(Value::Int64(1), DataType::Int64),
            const_expr(Value::Int64(2), DataType::Int64),
            const_expr(Value::Int64(3), DataType::Int64),
        ]),
        DataType::Array(Box::new(DataType::Int64)),
    );
    assert_eq!(
        eval_typed_expr(&expr, &row, &qctx).unwrap(),
        Value::Array(vec![Value::Int64(1), Value::Int64(2), Value::Int64(3)])
    );

    // Nested: ARRAY[ARRAY[1, 2], ARRAY[3, 4]]
    let nested = TypedExpr::new(
        TypedExprKind::ArrayLiteral(vec![
            TypedExpr::new(
                TypedExprKind::ArrayLiteral(vec![
                    const_expr(Value::Int64(1), DataType::Int64),
                    const_expr(Value::Int64(2), DataType::Int64),
                ]),
                DataType::Array(Box::new(DataType::Int64)),
            ),
            TypedExpr::new(
                TypedExprKind::ArrayLiteral(vec![
                    const_expr(Value::Int64(3), DataType::Int64),
                    const_expr(Value::Int64(4), DataType::Int64),
                ]),
                DataType::Array(Box::new(DataType::Int64)),
            ),
        ]),
        DataType::Array(Box::new(DataType::Array(Box::new(DataType::Int64)))),
    );
    let result = eval_typed_expr(&nested, &row, &qctx).unwrap();
    assert_eq!(
        result,
        Value::Array(vec![
            Value::Array(vec![Value::Int64(1), Value::Int64(2)]),
            Value::Array(vec![Value::Int64(3), Value::Int64(4)]),
        ])
    );
}

#[test]
fn test_json_access_arrow() {
    let row = empty_row();
    let qctx = test_qctx();
    // '{"a": 1}' -> 'a' → JSON '1'
    let expr = TypedExpr::new(
        TypedExprKind::JsonAccess {
            expr: Box::new(const_expr(
                Value::Jsonb(r#"{"a": 1}"#.into()),
                DataType::Jsonb,
            )),
            path: Box::new(const_expr(Value::Text("a".into()), DataType::Text)),
            operator: JsonAccessOp::Arrow,
        },
        DataType::Jsonb,
    );
    let result = eval_typed_expr(&expr, &row, &qctx).unwrap();
    // -> returns JSON value
    assert!(
        matches!(&result, Value::Jsonb(_) | Value::Json(_) | Value::Text(_)),
        "expected JSON-like result, got {:?}",
        result
    );

    // '{"a": 1}' ->> 'a' → Text '1'
    let text_expr = TypedExpr::new(
        TypedExprKind::JsonAccess {
            expr: Box::new(const_expr(
                Value::Jsonb(r#"{"a": 1}"#.into()),
                DataType::Jsonb,
            )),
            path: Box::new(const_expr(Value::Text("a".into()), DataType::Text)),
            operator: JsonAccessOp::LongArrow,
        },
        DataType::Text,
    );
    let result = eval_typed_expr(&text_expr, &row, &qctx).unwrap();
    assert_eq!(result, Value::Text("1".into()));
}

#[test]
fn test_greatest_least_with_nulls() {
    let row = empty_row();
    let qctx = test_qctx();
    // GREATEST(NULL, 3, 1, NULL, 5) → 5
    let greatest = TypedExpr::new(
        TypedExprKind::MinMax {
            args: vec![
                const_expr(Value::Null, DataType::Int64),
                const_expr(Value::Int64(3), DataType::Int64),
                const_expr(Value::Int64(1), DataType::Int64),
                const_expr(Value::Null, DataType::Int64),
                const_expr(Value::Int64(5), DataType::Int64),
            ],
            is_greatest: true,
        },
        DataType::Int64,
    );
    assert_eq!(
        eval_typed_expr(&greatest, &row, &qctx).unwrap(),
        Value::Int64(5)
    );

    // LEAST(NULL, 3, 1, NULL, 5) → 1
    let least = TypedExpr::new(
        TypedExprKind::MinMax {
            args: vec![
                const_expr(Value::Null, DataType::Int64),
                const_expr(Value::Int64(3), DataType::Int64),
                const_expr(Value::Int64(1), DataType::Int64),
                const_expr(Value::Null, DataType::Int64),
                const_expr(Value::Int64(5), DataType::Int64),
            ],
            is_greatest: false,
        },
        DataType::Int64,
    );
    assert_eq!(
        eval_typed_expr(&least, &row, &qctx).unwrap(),
        Value::Int64(1)
    );

    // GREATEST(NULL, NULL) → NULL
    let all_null = TypedExpr::new(
        TypedExprKind::MinMax {
            args: vec![
                const_expr(Value::Null, DataType::Int64),
                const_expr(Value::Null, DataType::Int64),
            ],
            is_greatest: true,
        },
        DataType::Int64,
    );
    assert_eq!(
        eval_typed_expr(&all_null, &row, &qctx).unwrap(),
        Value::Null
    );
}
