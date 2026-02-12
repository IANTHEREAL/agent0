use super::super::statement_time;
use super::*;
use crate::types::ColumnDef;
use rust_decimal::Decimal;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use std::collections::HashMap;
use std::str::FromStr;

fn parse_expr(sql: &str) -> Expr {
    let full_sql = format!("SELECT {}", sql);
    let dialect = PostgreSqlDialect {};
    let ast = Parser::parse_sql(&dialect, &full_sql).unwrap();
    if let sqlparser::ast::Statement::Query(q) = &ast[0] {
        if let sqlparser::ast::SetExpr::Select(s) = &*q.body {
            if let sqlparser::ast::SelectItem::UnnamedExpr(e) = &s.projection[0] {
                return e.clone();
            }
        }
    }
    panic!("Failed to parse expression");
}

#[test]
fn test_version_includes_pg_tikv() {
    let v = eval_expr(&parse_expr("version()"), None, None).unwrap();
    let Value::Text(s) = v else {
        panic!("version() must return text");
    };
    assert!(s.starts_with("PostgreSQL "));
    assert!(s.contains("pg-tikv "));
}

#[test]
fn test_eval_expr_join_function_resolves_qualified_column() {
    let expr = parse_expr("LOWER(b.name)");

    let combined_schema = TableSchema {
        name: "join".to_string(),
        columns: vec![
            ColumnDef {
                name: "name".to_string(),
                data_type: DataType::Text,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            },
            ColumnDef {
                name: "name".to_string(),
                data_type: DataType::Text,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            },
        ],
        ..Default::default()
    };

    let combined_row = Row::new(vec![
        Value::Text("Alice".to_string()),
        Value::Text("Bob".to_string()),
    ]);

    let mut column_offsets = HashMap::new();
    column_offsets.insert("a.name".to_string(), 0);
    column_offsets.insert("b.name".to_string(), 1);
    column_offsets.insert("name".to_string(), 0);

    let ctx = JoinEvalContext::new(&column_offsets, None, &combined_row, &combined_schema);

    let val = eval_join_expr(&ctx, &expr).unwrap();
    assert_eq!(val, Value::Text("bob".to_string()));
}

#[test]
fn test_pg_get_indexdef_uses_qualified_indexdef_column() {
    let expr = parse_expr("pg_get_indexdef(ix.indexrelid)");

    let schema = TableSchema {
        name: "join_result".to_string(),
        columns: vec![
            ColumnDef {
                name: "ix.indexrelid".to_string(),
                data_type: DataType::Int64,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            },
            ColumnDef {
                name: "ix.indexdef".to_string(),
                data_type: DataType::Text,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            },
        ],
        ..Default::default()
    };

    let expected = "CREATE INDEX ix_t_a ON public.ix_t USING btree (a)";
    let row = Row::new(vec![Value::Int64(42), Value::Text(expected.to_string())]);

    let val = eval_expr(&expr, Some(&row), Some(&schema)).unwrap();
    assert_eq!(val, Value::Text(expected.to_string()));
}

#[test]
fn test_pg_get_constraintdef_uses_qualified_constraintdef_column() {
    let expr = parse_expr("pg_get_constraintdef(con.oid)");

    let schema = TableSchema {
        name: "join_result".to_string(),
        columns: vec![
            ColumnDef {
                name: "con.oid".to_string(),
                data_type: DataType::Int64,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            },
            ColumnDef {
                name: "con.constraintdef".to_string(),
                data_type: DataType::Text,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            },
        ],
        ..Default::default()
    };

    let expected = "PRIMARY KEY (id)";
    let row = Row::new(vec![Value::Int64(50001), Value::Text(expected.to_string())]);

    let val = eval_expr(&expr, Some(&row), Some(&schema)).unwrap();
    assert_eq!(val, Value::Text(expected.to_string()));
}

#[test]
fn test_eval_literal_values() {
    assert_eq!(
        eval_expr(&parse_expr("42"), None, None).unwrap(),
        Value::Int32(42)
    );
    assert_eq!(
        eval_expr(&parse_expr("3.14"), None, None).unwrap(),
        Value::Numeric(Decimal::from_str("3.14").unwrap())
    );
    assert_eq!(
        eval_expr(&parse_expr("'hello'"), None, None).unwrap(),
        Value::Text("hello".to_string())
    );
    assert_eq!(
        eval_expr(&parse_expr("$$hello$$"), None, None).unwrap(),
        Value::Text("hello".to_string())
    );
    assert_eq!(
        eval_expr(&parse_expr("$tag$hello$tag$"), None, None).unwrap(),
        Value::Text("hello".to_string())
    );
    assert_eq!(
        eval_expr(&parse_expr("$$ $1 $$"), None, None).unwrap(),
        Value::Text(" $1 ".to_string())
    );
    assert_eq!(
        eval_expr(&parse_expr("NULL"), None, None).unwrap(),
        Value::Null
    );
    assert_eq!(
        eval_expr(&parse_expr("true"), None, None).unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(
        eval_expr(&parse_expr("false"), None, None).unwrap(),
        Value::Boolean(false)
    );
}

#[test]
fn test_at_time_zone_timestamp_to_timestamptz() {
    let expr = parse_expr("TIMESTAMP '2024-01-15 10:00:00' AT TIME ZONE 'UTC'");
    let val = eval_expr(&expr, None, None).unwrap();
    assert_eq!(val, parse_timestamp_string("2024-01-15 10:00:00").unwrap());
}

#[test]
fn test_at_time_zone_timestamp_to_timestamptz_with_offset() {
    let expr = parse_expr("TIMESTAMP '2024-01-15 10:00:00' AT TIME ZONE 'Asia/Shanghai'");
    let val = eval_expr(&expr, None, None).unwrap();
    assert_eq!(val, parse_timestamp_string("2024-01-15 02:00:00").unwrap());
}

#[test]
fn test_at_time_zone_chain_conversion() {
    let expr = parse_expr(
        "TIMESTAMP '2024-01-15 10:00:00' AT TIME ZONE 'UTC' AT TIME ZONE 'America/New_York'",
    );
    let val = eval_expr(&expr, None, None).unwrap();
    assert_eq!(val, parse_timestamp_string("2024-01-15 05:00:00").unwrap());
}

#[test]
fn test_at_time_zone_timestamptz_to_timestamp() {
    let expr = parse_expr("TIMESTAMPTZ '2024-01-15T10:00:00Z' AT TIME ZONE 'America/New_York'");
    let val = eval_expr(&expr, None, None).unwrap();
    assert_eq!(val, parse_timestamp_string("2024-01-15 05:00:00").unwrap());
}

#[test]
fn test_eval_arithmetic() {
    assert_eq!(
        eval_expr(&parse_expr("1 + 2"), None, None).unwrap(),
        Value::Int32(3)
    );
    assert_eq!(
        eval_expr(&parse_expr("10 - 4"), None, None).unwrap(),
        Value::Int32(6)
    );
    assert_eq!(
        eval_expr(&parse_expr("3 * 5"), None, None).unwrap(),
        Value::Int32(15)
    );
    assert_eq!(
        eval_expr(&parse_expr("20 / 4"), None, None).unwrap(),
        Value::Int32(5)
    );
    assert_eq!(
        eval_expr(&parse_expr("17 % 5"), None, None).unwrap(),
        Value::Int32(2)
    );
}

#[test]
fn test_eval_comparison() {
    assert_eq!(
        eval_expr(&parse_expr("5 > 3"), None, None).unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(
        eval_expr(&parse_expr("5 < 3"), None, None).unwrap(),
        Value::Boolean(false)
    );
    assert_eq!(
        eval_expr(&parse_expr("5 = 5"), None, None).unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(
        eval_expr(&parse_expr("5 <> 3"), None, None).unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(
        eval_expr(&parse_expr("5 >= 5"), None, None).unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(
        eval_expr(&parse_expr("5 <= 6"), None, None).unwrap(),
        Value::Boolean(true)
    );
}

#[test]
fn test_eval_logical() {
    assert_eq!(
        eval_expr(&parse_expr("true AND true"), None, None).unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(
        eval_expr(&parse_expr("true AND false"), None, None).unwrap(),
        Value::Boolean(false)
    );
    assert_eq!(
        eval_expr(&parse_expr("true OR false"), None, None).unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(
        eval_expr(&parse_expr("false OR false"), None, None).unwrap(),
        Value::Boolean(false)
    );
    assert_eq!(
        eval_expr(&parse_expr("NOT true"), None, None).unwrap(),
        Value::Boolean(false)
    );
    assert_eq!(
        eval_expr(&parse_expr("NOT false"), None, None).unwrap(),
        Value::Boolean(true)
    );
}

#[test]
fn test_eval_logical_null_semantics_and_short_circuit() {
    assert_eq!(
        eval_expr(&parse_expr("CAST(NULL AS BOOLEAN) AND TRUE"), None, None).unwrap(),
        Value::Null
    );
    assert_eq!(
        eval_expr(&parse_expr("FALSE AND CAST(NULL AS BOOLEAN)"), None, None).unwrap(),
        Value::Boolean(false)
    );
    assert_eq!(
        eval_expr(&parse_expr("CAST(NULL AS BOOLEAN) OR TRUE"), None, None).unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(
        eval_expr(&parse_expr("NOT CAST(NULL AS BOOLEAN)"), None, None).unwrap(),
        Value::Null
    );

    // Short-circuit: RHS must not be evaluated when LHS determines result
    assert_eq!(
        eval_expr(&parse_expr("FALSE AND (1 / 0 = 0)"), None, None).unwrap(),
        Value::Boolean(false)
    );
    assert_eq!(
        eval_expr(&parse_expr("TRUE OR (1 / 0 = 0)"), None, None).unwrap(),
        Value::Boolean(true)
    );
}

#[test]
fn test_eval_logical_short_circuit_does_not_hide_type_errors() {
    // Short-circuit must not mask RHS type errors.
    assert!(eval_expr(&parse_expr("TRUE OR 42"), None, None).is_err());
    assert!(eval_expr(&parse_expr("FALSE AND 1 / 0"), None, None).is_err());
    assert!(eval_expr(&parse_expr("TRUE OR (FALSE AND 42)"), None, None).is_err());
    assert!(eval_expr(&parse_expr("FALSE AND (TRUE OR 42)"), None, None).is_err());
    assert!(eval_expr(&parse_expr("TRUE OR (1 LIKE 'a%')"), None, None)
        .unwrap_err()
        .to_string()
        .contains("LIKE requires text operands"));
    assert!(eval_expr(
        &parse_expr("TRUE OR (CASE WHEN 1 LIKE 'a%' THEN TRUE ELSE FALSE END)"),
        None,
        None
    )
    .unwrap_err()
    .to_string()
    .contains("LIKE requires text operands"));
    assert!(
        eval_expr(&parse_expr("FALSE AND (1 ILIKE 'a%')"), None, None)
            .unwrap_err()
            .to_string()
            .contains("ILIKE requires text operands")
    );
    assert!(eval_expr(&parse_expr("TRUE OR (1 && 2)"), None, None)
        .unwrap_err()
        .to_string()
        .contains("&& operator requires array operands"));

    // Explicit NULL is allowed as a boolean operand.
    assert_eq!(
        eval_expr(&parse_expr("FALSE AND NULL"), None, None).unwrap(),
        Value::Boolean(false)
    );
    assert_eq!(
        eval_expr(&parse_expr("TRUE OR NULL"), None, None).unwrap(),
        Value::Boolean(true)
    );
}

#[test]
fn test_eval_nested() {
    assert_eq!(
        eval_expr(&parse_expr("(1 + 2) * 3"), None, None).unwrap(),
        Value::Int32(9)
    );
    assert_eq!(
        eval_expr(&parse_expr("10 / (2 + 3)"), None, None).unwrap(),
        Value::Int32(2)
    );
}

#[test]
fn test_eval_unary_minus() {
    assert_eq!(
        eval_expr(&parse_expr("-5"), None, None).unwrap(),
        Value::Int32(-5)
    );
    assert_eq!(
        eval_expr(&parse_expr("-3.14"), None, None).unwrap(),
        Value::Numeric(Decimal::from_str("-3.14").unwrap())
    );
    assert_eq!(
        eval_expr(&parse_expr("-'10'"), None, None).unwrap(),
        Value::Int32(-10)
    );
    assert_eq!(
        eval_expr(&parse_expr("-'1.5'"), None, None).unwrap(),
        Value::Float64(-1.5)
    );
    assert!(eval_expr(&parse_expr("-'nope'"), None, None).is_err());
}

#[test]
fn test_eval_is_null() {
    assert_eq!(
        eval_expr(&parse_expr("NULL IS NULL"), None, None).unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(
        eval_expr(&parse_expr("5 IS NULL"), None, None).unwrap(),
        Value::Boolean(false)
    );
    assert_eq!(
        eval_expr(&parse_expr("NULL IS NOT NULL"), None, None).unwrap(),
        Value::Boolean(false)
    );
    assert_eq!(
        eval_expr(&parse_expr("5 IS NOT NULL"), None, None).unwrap(),
        Value::Boolean(true)
    );
}

#[test]
fn test_eval_in_list() {
    assert_eq!(
        eval_expr(&parse_expr("5 IN (1, 3, 5, 7)"), None, None).unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(
        eval_expr(&parse_expr("4 IN (1, 3, 5, 7)"), None, None).unwrap(),
        Value::Boolean(false)
    );
    assert_eq!(
        eval_expr(&parse_expr("4 NOT IN (1, 3, 5, 7)"), None, None).unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(
        eval_expr(&parse_expr("'a' IN ('a', 'b', 'c')"), None, None).unwrap(),
        Value::Boolean(true)
    );
}

#[test]
fn test_eval_between() {
    assert_eq!(
        eval_expr(&parse_expr("5 BETWEEN 1 AND 10"), None, None).unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(
        eval_expr(&parse_expr("15 BETWEEN 1 AND 10"), None, None).unwrap(),
        Value::Boolean(false)
    );
    assert_eq!(
        eval_expr(&parse_expr("5 NOT BETWEEN 10 AND 20"), None, None).unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(
        eval_expr(&parse_expr("1 BETWEEN 1 AND 1"), None, None).unwrap(),
        Value::Boolean(true)
    );
}

#[test]
fn test_compare_values() {
    assert_eq!(
        compare_values(&Value::Int32(5), &Value::Int32(3)).unwrap(),
        1
    );
    assert_eq!(
        compare_values(&Value::Int32(3), &Value::Int32(5)).unwrap(),
        -1
    );
    assert_eq!(
        compare_values(&Value::Int32(5), &Value::Int32(5)).unwrap(),
        0
    );
    assert_eq!(
        compare_values(&Value::Text("b".to_string()), &Value::Text("a".to_string())).unwrap(),
        1
    );
    assert_eq!(
        compare_values(&Value::Bytes(vec![0x00]), &Value::Bytes(vec![0x01])).unwrap(),
        -1
    );
    assert_eq!(
        compare_values(&Value::Bytes(vec![0x01, 0x00]), &Value::Bytes(vec![0x01])).unwrap(),
        1
    );
    assert_eq!(
        compare_values(
            &Value::Bytes(vec![0xde, 0xad]),
            &Value::Bytes(vec![0xde, 0xad])
        )
        .unwrap(),
        0
    );
    assert_eq!(compare_values(&Value::Null, &Value::Int32(5)).unwrap(), -1);
    assert_eq!(compare_values(&Value::Int32(5), &Value::Null).unwrap(), 1);

    assert_eq!(
        compare_values(&Value::Boolean(true), &Value::Text("true".to_string())).unwrap(),
        0
    );
    assert_eq!(
        compare_values(&Value::Text("false".to_string()), &Value::Boolean(false)).unwrap(),
        0
    );
    assert!(compare_values(&Value::Boolean(true), &Value::Text("nope".to_string())).is_err());

    // PostgreSQL-like float NaN semantics:
    // - NaN compares equal to NaN
    // - NaN compares greater than all non-NaN values
    assert_eq!(
        compare_values(&Value::Float64(f64::NAN), &Value::Float64(1.0)).unwrap(),
        1
    );
    assert_eq!(
        compare_values(&Value::Float64(1.0), &Value::Float64(f64::NAN)).unwrap(),
        -1
    );
    assert_eq!(
        compare_values(&Value::Float64(f64::NAN), &Value::Float64(f64::NAN)).unwrap(),
        0
    );
    assert_eq!(
        compare_values(&Value::Float64(f64::NAN), &Value::Int32(1)).unwrap(),
        1
    );
    assert_eq!(
        compare_values(&Value::Int32(1), &Value::Float64(f64::NAN)).unwrap(),
        -1
    );
}

#[test]
fn test_float_nan_comparisons() {
    assert_eq!(
        eval_expr(
            &parse_expr("CAST('NaN' AS DOUBLE PRECISION) = 1"),
            None,
            None
        )
        .unwrap(),
        Value::Boolean(false)
    );
    assert_eq!(
        eval_expr(
            &parse_expr("CAST('NaN' AS DOUBLE PRECISION) = CAST('NaN' AS DOUBLE PRECISION)"),
            None,
            None
        )
        .unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(
        eval_expr(
            &parse_expr("CAST('NaN' AS DOUBLE PRECISION) > 1"),
            None,
            None
        )
        .unwrap(),
        Value::Boolean(true)
    );
}

#[test]
fn test_compare_values_nan_semantics() {
    let nan1 = f64::from_bits(0x7ff8_0000_0000_0001);
    let nan2 = f64::from_bits(0x7ff8_0000_0000_0002);
    assert!(nan1.is_nan() && nan2.is_nan());

    assert_eq!(
        compare_values(&Value::Float64(nan1), &Value::Float64(nan2)).unwrap(),
        0
    );
    assert_eq!(
        compare_values(&Value::Float64(nan1), &Value::Float64(1.0)).unwrap(),
        1
    );
    assert_eq!(
        compare_values(&Value::Float64(1.0), &Value::Float64(nan1)).unwrap(),
        -1
    );
}

#[test]
fn test_compare_order_by_values_nulls() {
    use std::cmp::Ordering;

    // ASC defaults to NULLS LAST.
    assert_eq!(
        compare_order_by_values(&Value::Null, &Value::Date(0), true, false).unwrap(),
        Ordering::Greater
    );
    assert_eq!(
        compare_order_by_values(&Value::Date(0), &Value::Null, true, false).unwrap(),
        Ordering::Less
    );

    // DESC defaults to NULLS FIRST.
    assert_eq!(
        compare_order_by_values(&Value::Null, &Value::Date(0), false, true).unwrap(),
        Ordering::Less
    );
    assert_eq!(
        compare_order_by_values(&Value::Date(0), &Value::Null, false, true).unwrap(),
        Ordering::Greater
    );
}

#[test]
fn test_compare_order_by_values_nan() {
    use std::cmp::Ordering;

    assert_eq!(
        compare_order_by_values(&Value::Float64(f64::NAN), &Value::Float64(1.0), true, false)
            .unwrap(),
        Ordering::Greater
    );
    assert_eq!(
        compare_order_by_values(&Value::Float64(1.0), &Value::Float64(f64::NAN), true, false)
            .unwrap(),
        Ordering::Less
    );

    // DESC should put NaN first (since NaN is treated as greatest).
    assert_eq!(
        compare_order_by_values(
            &Value::Float64(f64::NAN),
            &Value::Float64(1.0),
            false,
            false
        )
        .unwrap(),
        Ordering::Less
    );
}

#[test]
fn test_jsonb_exists_function() {
    assert_eq!(
        eval_expr(
            &parse_expr("JSONB_EXISTS('{\"a\": 1, \"b\": 2}'::jsonb, 'a')"),
            None,
            None
        )
        .unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(
        eval_expr(
            &parse_expr("JSONB_EXISTS('{\"a\": 1}'::jsonb, 'c')"),
            None,
            None
        )
        .unwrap(),
        Value::Boolean(false)
    );
    assert_eq!(
        eval_expr(
            &parse_expr("JSONB_EXISTS('[\"a\", \"b\"]'::jsonb, 'b')"),
            None,
            None
        )
        .unwrap(),
        Value::Boolean(true)
    );
}

#[test]
fn test_to_char_format_tokens() {
    assert_eq!(
        eval_expr(
            &parse_expr("TO_CHAR(TIMESTAMP '2024-01-15 14:30:45', 'YYYY-MM')"),
            None,
            None
        )
        .unwrap(),
        Value::Text("2024-01".to_string())
    );
    assert_eq!(
        eval_expr(
            &parse_expr("TO_CHAR(DATE '2024-01-15', 'YYYY-MM')"),
            None,
            None
        )
        .unwrap(),
        Value::Text("2024-01".to_string())
    );
    assert_eq!(
        eval_expr(
            &parse_expr("TO_CHAR(TIMESTAMP '2024-01-15 14:30:45', 'YYYY-MM-DD HH24:MI:SS')"),
            None,
            None
        )
        .unwrap(),
        Value::Text("2024-01-15 14:30:45".to_string())
    );
}

#[test]
fn test_null_comparison_three_valued_logic() {
    assert_eq!(
        eval_expr(&parse_expr("NULL = 5"), None, None).unwrap(),
        Value::Null
    );
    assert_eq!(
        eval_expr(&parse_expr("5 = NULL"), None, None).unwrap(),
        Value::Null
    );
    assert_eq!(
        eval_expr(&parse_expr("NULL >= 0"), None, None).unwrap(),
        Value::Null
    );
    assert_eq!(
        eval_expr(&parse_expr("NULL < 10"), None, None).unwrap(),
        Value::Null
    );
    assert_eq!(
        eval_expr(&parse_expr("NULL <> 5"), None, None).unwrap(),
        Value::Null
    );
}

#[test]
fn test_division_by_zero() {
    assert!(eval_expr(&parse_expr("5 / 0"), None, None).is_err());
    assert!(eval_expr(&parse_expr("5 % 0"), None, None).is_err());
}

#[test]
fn test_function_args_do_not_drop_errors() {
    assert!(eval_expr(&parse_expr("COALESCE(5 / 0, 1)"), None, None).is_err());
    assert!(eval_expr(&parse_expr("COALESCE(NULL, 5 / 0, 1)"), None, None).is_err());
    assert_eq!(
        eval_expr(&parse_expr("COALESCE(1, 5 / 0)"), None, None).unwrap(),
        Value::Int32(1)
    );
    assert!(eval_expr(&parse_expr("NULLIF(5 / 0, 1)"), None, None).is_err());
    assert!(eval_expr(&parse_expr("GREATEST(1, 5 / 0)"), None, None).is_err());
}

#[test]
fn test_function_column_references_use_row_context() {
    use crate::types::ColumnDef;

    let schema = TableSchema::new(
        "public.atm_users".to_string(),
        1,
        vec![ColumnDef {
            name: "nickname".to_string(),
            data_type: DataType::Text,
            nullable: true,
            primary_key: false,
            unique: false,
            is_serial: false,
            default_expr: None,
        }],
        vec![],
    );
    let row = Row::new(vec![Value::Null]);
    assert_eq!(
        eval_expr(
            &parse_expr("COALESCE(nickname, 'NULL')"),
            Some(&row),
            Some(&schema),
        )
        .unwrap(),
        Value::Text("NULL".to_string())
    );

    let row_with_value = Row::new(vec![Value::Text("hi".to_string())]);
    assert_eq!(
        eval_expr(
            &parse_expr("COALESCE(nickname, 'NULL')"),
            Some(&row_with_value),
            Some(&schema)
        )
        .unwrap(),
        Value::Text("hi".to_string())
    );
}

#[test]
fn test_mixed_type_arithmetic() {
    let result = eval_expr(&parse_expr("1 + 2.5"), None, None).unwrap();
    assert_eq!(result, Value::Numeric(Decimal::from_str("3.5").unwrap()));
}

#[test]
fn test_string_concat() {
    assert_eq!(
        eval_expr(&parse_expr("'Hello' || ' ' || 'World'"), None, None).unwrap(),
        Value::Text("Hello World".to_string())
    );
    assert_eq!(
        eval_expr(&parse_expr("'Count: ' || 42"), None, None).unwrap(),
        Value::Text("Count: 42".to_string())
    );
}

#[test]
fn test_case_when() {
    assert_eq!(
        eval_expr(
            &parse_expr("CASE WHEN 1 = 1 THEN 'yes' ELSE 'no' END"),
            None,
            None
        )
        .unwrap(),
        Value::Text("yes".to_string())
    );
    assert_eq!(
        eval_expr(
            &parse_expr("CASE WHEN 1 = 2 THEN 'yes' ELSE 'no' END"),
            None,
            None
        )
        .unwrap(),
        Value::Text("no".to_string())
    );
    assert_eq!(
        eval_expr(
            &parse_expr("CASE WHEN 'true' THEN 'yes' ELSE 'no' END"),
            None,
            None
        )
        .unwrap(),
        Value::Text("yes".to_string())
    );
    assert_eq!(
        eval_expr(
            &parse_expr("CASE WHEN 'false' THEN 'yes' ELSE 'no' END"),
            None,
            None
        )
        .unwrap(),
        Value::Text("no".to_string())
    );
    assert!(eval_expr(
        &parse_expr("CASE WHEN 'nope' THEN 'yes' ELSE 'no' END"),
        None,
        None
    )
    .is_err());
    assert_eq!(
        eval_expr(
            &parse_expr("CASE 2 WHEN 1 THEN 'one' WHEN 2 THEN 'two' ELSE 'other' END"),
            None,
            None
        )
        .unwrap(),
        Value::Text("two".to_string())
    );
}

#[test]
fn test_string_functions() {
    assert_eq!(
        eval_expr(&parse_expr("UPPER('hello')"), None, None).unwrap(),
        Value::Text("HELLO".to_string())
    );
    assert_eq!(
        eval_expr(&parse_expr("LOWER('HELLO')"), None, None).unwrap(),
        Value::Text("hello".to_string())
    );
    assert_eq!(
        eval_expr(&parse_expr("LENGTH('hello')"), None, None).unwrap(),
        Value::Int32(5)
    );
    assert_eq!(
        eval_expr(&parse_expr("CONCAT('a', 'b', 'c')"), None, None).unwrap(),
        Value::Text("abc".to_string())
    );
    assert_eq!(
        eval_expr(&parse_expr("LEFT('hello', 2)"), None, None).unwrap(),
        Value::Text("he".to_string())
    );
    assert_eq!(
        eval_expr(&parse_expr("RIGHT('hello', 2)"), None, None).unwrap(),
        Value::Text("lo".to_string())
    );
    assert_eq!(
        eval_expr(
            &parse_expr("REPLACE('hello world', 'world', 'there')"),
            None,
            None
        )
        .unwrap(),
        Value::Text("hello there".to_string())
    );
    assert_eq!(
        eval_expr(&parse_expr("REVERSE('hello')"), None, None).unwrap(),
        Value::Text("olleh".to_string())
    );
    assert_eq!(
        eval_expr(&parse_expr("REPEAT('ab', 3)"), None, None).unwrap(),
        Value::Text("ababab".to_string())
    );
}

#[test]
fn test_math_functions() {
    assert_eq!(
        eval_expr(&parse_expr("ABS(-5)"), None, None).unwrap(),
        Value::Int32(5)
    );
    assert_eq!(
        eval_expr(&parse_expr("CEIL(4.3)"), None, None).unwrap(),
        Value::Float64(5.0)
    );
    assert_eq!(
        eval_expr(&parse_expr("FLOOR(4.7)"), None, None).unwrap(),
        Value::Float64(4.0)
    );
    let round_result = eval_expr(&parse_expr("ROUND(4.567, 2)"), None, None).unwrap();
    assert!(matches!(round_result, Value::Float64(f) if (f - 4.57).abs() < 0.001));
    assert_eq!(
        eval_expr(&parse_expr("SQRT(16)"), None, None).unwrap(),
        Value::Float64(4.0)
    );
    assert_eq!(
        eval_expr(&parse_expr("POWER(2, 10)"), None, None).unwrap(),
        Value::Float64(1024.0)
    );
    assert_eq!(
        eval_expr(&parse_expr("MOD(17, 5)"), None, None).unwrap(),
        Value::Int32(2)
    );
    assert_eq!(
        eval_expr(&parse_expr("SIGN(-5)"), None, None).unwrap(),
        Value::Int32(-1)
    );
}

#[test]
fn test_coalesce_nullif() {
    assert_eq!(
        eval_expr(&parse_expr("COALESCE(NULL, NULL, 'default')"), None, None).unwrap(),
        Value::Text("default".to_string())
    );
    assert_eq!(
        eval_expr(
            &parse_expr("COALESCE('first', NULL, 'default')"),
            None,
            None
        )
        .unwrap(),
        Value::Text("first".to_string())
    );
    assert_eq!(
        eval_expr(&parse_expr("NULLIF(5, 5)"), None, None).unwrap(),
        Value::Null
    );
    assert_eq!(
        eval_expr(&parse_expr("NULLIF(5, 3)"), None, None).unwrap(),
        Value::Int32(5)
    );
}

#[test]
fn test_greatest_least() {
    assert_eq!(
        eval_expr(&parse_expr("GREATEST(1, 5, 3)"), None, None).unwrap(),
        Value::Int32(5)
    );
    assert_eq!(
        eval_expr(&parse_expr("LEAST(1, 5, 3)"), None, None).unwrap(),
        Value::Int32(1)
    );
}

#[test]
fn test_like_pattern() {
    assert_eq!(
        eval_expr(&parse_expr("'hello' LIKE 'h%'"), None, None).unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(
        eval_expr(&parse_expr("'hello' LIKE '%llo'"), None, None).unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(
        eval_expr(&parse_expr("'hello' LIKE 'h_llo'"), None, None).unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(
        eval_expr(&parse_expr("'hello' LIKE 'world'"), None, None).unwrap(),
        Value::Boolean(false)
    );
    assert_eq!(
        eval_expr(&parse_expr("'hello' NOT LIKE 'world'"), None, None).unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(
        eval_expr(&parse_expr("'hello' LIKE '%.%'"), None, None).unwrap(),
        Value::Boolean(false)
    );
    assert_eq!(
        eval_expr(&parse_expr("'a.b' LIKE '%.%'"), None, None).unwrap(),
        Value::Boolean(true)
    );
}

#[test]
fn test_like_null_semantics() {
    assert_eq!(
        eval_expr(&parse_expr("CAST(NULL AS TEXT) LIKE 'a%'"), None, None).unwrap(),
        Value::Null
    );
    assert_eq!(
        eval_expr(&parse_expr("'a' LIKE CAST(NULL AS TEXT)"), None, None).unwrap(),
        Value::Null
    );
    assert_eq!(
        eval_expr(&parse_expr("CAST(NULL AS TEXT) ILIKE 'a%'"), None, None).unwrap(),
        Value::Null
    );
    assert_eq!(
        eval_expr(&parse_expr("'a' ILIKE CAST(NULL AS TEXT)"), None, None).unwrap(),
        Value::Null
    );
    assert!(eval_expr(&parse_expr("1 LIKE NULL"), None, None).is_err());
    assert!(eval_expr(&parse_expr("NULL LIKE 1"), None, None).is_err());
}

#[test]
fn test_ilike_pattern() {
    assert_eq!(
        eval_expr(&parse_expr("'Hello' ILIKE 'h%'"), None, None).unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(
        eval_expr(&parse_expr("'HELLO' ILIKE '%llo'"), None, None).unwrap(),
        Value::Boolean(true)
    );
}

#[test]
fn test_similar_to_trailing_escape() {
    // Issue #545: Pattern ending with escape character should not match
    assert_eq!(
        eval_expr(
            &parse_expr("'123A_' SIMILAR TO '%A_' ESCAPE '_'"),
            None,
            None
        )
        .unwrap(),
        Value::Boolean(false)
    );

    // Escaped underscore followed by literal underscore should match
    assert_eq!(
        eval_expr(
            &parse_expr("'123A_' SIMILAR TO '%A__' ESCAPE '_'"),
            None,
            None
        )
        .unwrap(),
        Value::Boolean(true)
    );
}

#[test]
fn test_cast() {
    assert_eq!(
        eval_expr(&parse_expr("CAST(123 AS TEXT)"), None, None).unwrap(),
        Value::Text("123".to_string())
    );
    assert_eq!(
        eval_expr(&parse_expr("CAST('456' AS INTEGER)"), None, None).unwrap(),
        Value::Int32(456)
    );
    assert_eq!(
        eval_expr(&parse_expr("CAST(3.14 AS INTEGER)"), None, None).unwrap(),
        Value::Int32(3)
    );
    assert_eq!(
        eval_expr(&parse_expr("'123'::int8"), None, None).unwrap(),
        Value::Int64(123)
    );
    assert_eq!(
        eval_expr(&parse_expr("'456'::bigint"), None, None).unwrap(),
        Value::Int64(456)
    );
    assert_eq!(
        eval_expr(&parse_expr("123::text"), None, None).unwrap(),
        Value::Text("123".to_string())
    );
}

#[test]
fn test_trim() {
    assert_eq!(
        eval_expr(&parse_expr("TRIM('  hello  ')"), None, None).unwrap(),
        Value::Text("hello".to_string())
    );
}

#[test]
fn test_position() {
    assert_eq!(
        eval_expr(&parse_expr("POSITION('lo' IN 'hello')"), None, None).unwrap(),
        Value::Int32(4)
    );
    assert_eq!(
        eval_expr(&parse_expr("POSITION('xyz' IN 'hello')"), None, None).unwrap(),
        Value::Int32(0)
    );
}

#[test]
fn test_substring() {
    assert_eq!(
        eval_expr(&parse_expr("SUBSTRING('hello' FROM 2 FOR 3)"), None, None).unwrap(),
        Value::Text("ell".to_string())
    );
    assert_eq!(
        eval_expr(&parse_expr("SUBSTRING('hello' FROM 2)"), None, None).unwrap(),
        Value::Text("ello".to_string())
    );
}

#[test]
fn test_interval_parsing() {
    use crate::types::IntervalValue;
    let result = parse_interval_string("1 day").unwrap();
    assert_eq!(
        result,
        Value::Interval(IntervalValue::from_millis(24 * 60 * 60 * 1000))
    );

    let result = parse_interval_string("2 hours").unwrap();
    assert_eq!(
        result,
        Value::Interval(IntervalValue::from_millis(2 * 60 * 60 * 1000))
    );

    let result = parse_interval_string("30 minutes").unwrap();
    assert_eq!(
        result,
        Value::Interval(IntervalValue::from_millis(30 * 60 * 1000))
    );

    let result = parse_interval_string("1 week").unwrap();
    assert_eq!(
        result,
        Value::Interval(IntervalValue::from_millis(7 * 24 * 60 * 60 * 1000))
    );

    let result = parse_interval_string("1 month").unwrap();
    assert_eq!(result, Value::Interval(IntervalValue::from_months(1)));
}

#[test]
fn test_interval_expression() {
    use crate::types::IntervalValue;
    let result = eval_expr(&parse_expr("INTERVAL '1 day'"), None, None).unwrap();
    assert_eq!(
        result,
        Value::Interval(IntervalValue::from_millis(24 * 60 * 60 * 1000))
    );

    let result = eval_expr(&parse_expr("INTERVAL '2' DAY"), None, None).unwrap();
    assert_eq!(
        result,
        Value::Interval(IntervalValue::from_millis(2 * 24 * 60 * 60 * 1000))
    );

    let result = eval_expr(&parse_expr("INTERVAL '3' HOUR"), None, None).unwrap();
    assert_eq!(
        result,
        Value::Interval(IntervalValue::from_millis(3 * 60 * 60 * 1000))
    );

    let result = eval_expr(&parse_expr("INTERVAL '1' MONTH"), None, None).unwrap();
    assert_eq!(result, Value::Interval(IntervalValue::from_months(1)));
}

#[test]
fn test_interval_expression_month_out_of_range_errors() {
    let err = eval_expr(&parse_expr("INTERVAL '2147483648' MONTH"), None, None).unwrap_err();
    assert!(err.to_string().contains("Interval out of range"));

    let err = eval_expr(&parse_expr("INTERVAL '214748365' YEAR"), None, None).unwrap_err();
    assert!(err.to_string().contains("Interval out of range"));
}

#[test]
fn test_timestamp_interval_arithmetic() {
    use crate::types::IntervalValue;
    let ts = Value::Timestamp(1000 * 60 * 60 * 24);
    let iv = Value::Interval(IntervalValue::from_millis(1000 * 60 * 60));

    let result = operators::add_values(ts.clone(), iv.clone()).unwrap();
    assert_eq!(result, Value::Timestamp(1000 * 60 * 60 * 25));

    let result = operators::sub_values(ts.clone(), iv.clone()).unwrap();
    assert_eq!(result, Value::Timestamp(1000 * 60 * 60 * 23));
}

#[test]
fn test_timestamp_cast() {
    let result = parse_timestamp_string("2024-01-01 00:00:00").unwrap();
    assert!(matches!(result, Value::Timestamp(_)));

    let result = parse_timestamp_string("2024-01-01").unwrap();
    assert!(matches!(result, Value::Timestamp(_)));

    let result = parse_timestamp_string("2026-01-22T04:36:12.931807").unwrap();
    assert!(matches!(result, Value::Timestamp(_)));

    let result = parse_timestamp_string("2024-01-15T10:30:00").unwrap();
    assert!(matches!(result, Value::Timestamp(_)));
}

#[test]
fn test_timestamp_cast_accepts_postgres_timestamptz_offsets() {
    let expected = chrono::DateTime::parse_from_rfc3339("2026-02-02T23:39:52.850+00:00")
        .unwrap()
        .timestamp_millis();

    assert_eq!(
        parse_timestamp_string("2026-02-02 23:39:52.850 +00:00").unwrap(),
        Value::Timestamp(expected)
    );
    assert_eq!(
        parse_timestamp_string("2026-02-02 23:39:52.850+00:00").unwrap(),
        Value::Timestamp(expected)
    );
    assert_eq!(
        parse_timestamp_string("2026-02-02 23:39:52.850000+00:00").unwrap(),
        Value::Timestamp(expected)
    );

    let expected_plus2 = chrono::DateTime::parse_from_rfc3339("2026-02-02T21:39:52.850+00:00")
        .unwrap()
        .timestamp_millis();
    assert_eq!(
        parse_timestamp_string("2026-02-02 23:39:52.850 +02:00").unwrap(),
        Value::Timestamp(expected_plus2)
    );

    // `+HH` offsets are also accepted by PostgreSQL (interpreted as `+HH:00`).
    assert_eq!(
        parse_timestamp_string("2026-02-02 23:39:52.850 +02").unwrap(),
        Value::Timestamp(expected_plus2)
    );
}

#[test]
fn test_now_plus_interval() {
    let result = eval_expr(&parse_expr("NOW() + INTERVAL '1 DAY'"), None, None).unwrap();
    assert!(matches!(result, Value::Timestamp(_)));
}

#[test]
fn test_string_concat_to_interval() {
    use crate::types::IntervalValue;
    let result = eval_expr(&parse_expr("('1' || ' day')::interval"), None, None).unwrap();
    assert_eq!(
        result,
        Value::Interval(IntervalValue::from_millis(24 * 60 * 60 * 1000))
    );
}

#[test]
fn test_complex_datetime_expression() {
    let result = eval_expr(
        &parse_expr("now()::timestamp + ('1' || ' day')::interval"),
        None,
        None,
    )
    .unwrap();
    assert!(matches!(result, Value::Timestamp(_)));
}

#[test]
fn test_int8_cast_from_int() {
    assert_eq!(
        eval_expr(&parse_expr("42::int8"), None, None).unwrap(),
        Value::Int64(42)
    );
}

#[test]
fn test_int8_cast_from_text() {
    assert_eq!(
        eval_expr(&parse_expr("'999'::int8"), None, None).unwrap(),
        Value::Int64(999)
    );
}

#[test]
fn test_gen_random_uuid() {
    let result = eval_expr(&parse_expr("gen_random_uuid()"), None, None).unwrap();
    assert!(matches!(result, Value::Uuid(_)));
}

#[test]
fn test_uuid_cast_from_text() {
    let result = eval_expr(
        &parse_expr("'550e8400-e29b-41d4-a716-446655440000'::uuid"),
        None,
        None,
    )
    .unwrap();
    if let Value::Uuid(bytes) = result {
        let uuid = uuid::Uuid::from_bytes(bytes);
        assert_eq!(uuid.to_string(), "550e8400-e29b-41d4-a716-446655440000");
    } else {
        panic!("Expected UUID value");
    }
}

#[test]
fn test_bytea_send_functions() {
    assert_eq!(
        eval_expr(
            &parse_expr("int8send(72623859790382856::bigint)"),
            None,
            None
        )
        .unwrap(),
        Value::Bytes(vec![1, 2, 3, 4, 5, 6, 7, 8])
    );
    assert_eq!(
        eval_expr(&parse_expr("int4send(16909060)"), None, None).unwrap(),
        Value::Bytes(vec![1, 2, 3, 4])
    );

    let uuid = uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
    assert_eq!(
        eval_expr(
            &parse_expr("uuid_send('550e8400-e29b-41d4-a716-446655440000'::uuid)"),
            None,
            None
        )
        .unwrap(),
        Value::Bytes(uuid.as_bytes().to_vec())
    );
}

#[test]
fn test_set_bit_get_bit_bytea() {
    assert_eq!(
        eval_expr(&parse_expr(r"set_bit('\x00'::bytea, 0, 1)"), None, None).unwrap(),
        Value::Bytes(vec![0x80])
    );
    assert_eq!(
        eval_expr(&parse_expr(r"set_bit('\x00'::bytea, 7, 1)"), None, None).unwrap(),
        Value::Bytes(vec![0x01])
    );
    assert_eq!(
        eval_expr(&parse_expr(r"get_bit('\x80'::bytea, 0)"), None, None).unwrap(),
        Value::Int32(1)
    );
    assert_eq!(
        eval_expr(&parse_expr(r"get_bit('\x80'::bytea, 7)"), None, None).unwrap(),
        Value::Int32(0)
    );
}

#[test]
fn test_uuidv7_expression_components() {
    let expr = r#"encode(
        set_bit(
            set_bit(
                overlay(
                    uuid_send('550e8400-e29b-41d4-a716-446655440000'::uuid)
                    placing substring(int8send(1705312800000::bigint) from 3)
                    from 1 for 6
                ),
                52, 1
            ),
            53, 1
        ),
        'hex'
    )::uuid"#;

    let result = eval_expr(&parse_expr(expr), None, None).unwrap();
    let Value::Uuid(bytes) = result else {
        panic!("expected UUID result");
    };

    let base_uuid = uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
    let mut expected = base_uuid.as_bytes().to_vec();

    let ts: i64 = 1705312800000;
    let ts_bytes = ts.to_be_bytes();
    expected[..6].copy_from_slice(&ts_bytes[2..]);
    // uuidv7 sets version bits (52/53) to 1.
    expected[6] |= 0x0c;

    assert_eq!(bytes.as_slice(), expected.as_slice());
}

#[test]
fn test_encode_decode_escape() {
    assert_eq!(
        eval_expr(
            &parse_expr(r"encode('\x48656c6c6f'::bytea, 'escape')"),
            None,
            None
        )
        .unwrap(),
        Value::Text("Hello".to_string())
    );

    assert_eq!(
        eval_expr(&parse_expr("decode('Hello', 'escape')"), None, None).unwrap(),
        Value::Bytes(b"Hello".to_vec())
    );

    assert_eq!(
        eval_expr(&parse_expr(r"decode('\000', 'escape')"), None, None).unwrap(),
        Value::Bytes(vec![0])
    );
}

#[test]
fn test_decode_escape_invalid_sequence_errors() {
    assert!(eval_expr(&parse_expr(r"decode('\8', 'escape')"), None, None).is_err());
    assert!(eval_expr(&parse_expr(r"decode('\999', 'escape')"), None, None).is_err());
}

#[test]
fn test_json_arrow_object_key() {
    assert_eq!(
        eval_expr(
            &parse_expr(r#"'{"name": "Alice", "age": 30}' -> 'name'"#),
            None,
            None
        )
        .unwrap(),
        Value::Jsonb("\"Alice\"".to_string())
    );
}

#[test]
fn test_json_long_arrow_object_key() {
    assert_eq!(
        eval_expr(
            &parse_expr(r#"'{"name": "Alice", "age": 30}' ->> 'name'"#),
            None,
            None
        )
        .unwrap(),
        Value::Text("Alice".to_string())
    );
}

#[test]
fn test_json_arrow_array_index() {
    assert_eq!(
        eval_expr(&parse_expr(r#"'[1, 2, 3]' -> 0"#), None, None).unwrap(),
        Value::Jsonb("1".to_string())
    );
    assert_eq!(
        eval_expr(&parse_expr(r#"'["a", "b", "c"]' -> 1"#), None, None).unwrap(),
        Value::Jsonb("\"b\"".to_string())
    );
}

#[test]
fn test_json_long_arrow_array_index() {
    assert_eq!(
        eval_expr(&parse_expr(r#"'["a", "b", "c"]' ->> 1"#), None, None).unwrap(),
        Value::Text("b".to_string())
    );
}

#[test]
fn test_json_nested_access() {
    let intermediate = eval_expr(
        &parse_expr(r#"'{"user": {"name": "Bob"}}' -> 'user'"#),
        None,
        None,
    )
    .unwrap();
    assert_eq!(intermediate, Value::Jsonb("{\"name\":\"Bob\"}".to_string()));

    assert_eq!(
        eval_expr(&parse_expr(r#"'{"name": "Bob"}' ->> 'name'"#), None, None).unwrap(),
        Value::Text("Bob".to_string())
    );

    assert_eq!(
        eval_expr(
            &parse_expr(r#"'{"user": {"name": "Bob"}}' -> 'user' ->> 'name'"#),
            None,
            None
        )
        .unwrap(),
        Value::Text("Bob".to_string())
    );
}

#[test]
fn test_json_null_key() {
    assert_eq!(
        eval_expr(
            &parse_expr(r#"'{"name": "Alice"}' -> 'missing'"#),
            None,
            None
        )
        .unwrap(),
        Value::Null
    );
}

#[test]
fn test_json_number_extraction() {
    assert_eq!(
        eval_expr(&parse_expr(r#"'{"count": 42}' ->> 'count'"#), None, None).unwrap(),
        Value::Text("42".to_string())
    );
}

#[test]
fn test_array_literal() {
    assert_eq!(
        eval_expr(&parse_expr("ARRAY[1, 2, 3]"), None, None).unwrap(),
        Value::Array(vec![Value::Int32(1), Value::Int32(2), Value::Int32(3)])
    );
    assert_eq!(
        eval_expr(&parse_expr("ARRAY['a', 'b', 'c']"), None, None).unwrap(),
        Value::Array(vec![
            Value::Text("a".to_string()),
            Value::Text("b".to_string()),
            Value::Text("c".to_string())
        ])
    );
}

#[test]
fn test_array_subquery_preserves_array_dimensions() {
    assert_eq!(
        eval_expr(&parse_expr("ARRAY(SELECT ARRAY[1, 2])"), None, None).unwrap(),
        Value::Array(vec![Value::Array(vec![Value::Int32(1), Value::Int32(2)])])
    );
    assert_eq!(
        eval_expr(
            &parse_expr("ARRAY(SELECT string_to_array('a,b', ','))"),
            None,
            None
        )
        .unwrap(),
        Value::Array(vec![Value::Array(vec![
            Value::Text("a".to_string()),
            Value::Text("b".to_string()),
        ])])
    );
}

#[test]
fn test_array_subquery_flattens_set_returning_projection() {
    assert_eq!(
        eval_expr(
            &parse_expr(r#"ARRAY(SELECT jsonb_array_elements_text('["a","b"]'))"#),
            None,
            None
        )
        .unwrap(),
        Value::Array(vec![
            Value::Text("a".to_string()),
            Value::Text("b".to_string()),
        ])
    );
}

#[test]
fn test_array_subquery_flattens_set_returning_projection_nested_query() {
    assert_eq!(
        eval_expr(
            &parse_expr(r#"ARRAY((SELECT jsonb_array_elements_text('["a","b"]')))"#),
            None,
            None
        )
        .unwrap(),
        Value::Array(vec![
            Value::Text("a".to_string()),
            Value::Text("b".to_string()),
        ])
    );
}

#[test]
fn test_array_indexing() {
    assert_eq!(
        eval_expr(&parse_expr("(ARRAY[10, 20, 30])[2]"), None, None).unwrap(),
        Value::Int32(20)
    );
    assert_eq!(
        eval_expr(&parse_expr("(ARRAY['a', 'b', 'c'])[1]"), None, None).unwrap(),
        Value::Text("a".to_string())
    );
    assert_eq!(
        eval_expr(&parse_expr("(ARRAY[1, 2, 3])[5]"), None, None).unwrap(),
        Value::Null
    );
}

#[test]
fn test_array_length() {
    assert_eq!(
        eval_expr(&parse_expr("array_length(ARRAY[1, 2, 3], 1)"), None, None).unwrap(),
        Value::Int32(3)
    );
}

#[test]
fn test_array_position() {
    assert_eq!(
        eval_expr(
            &parse_expr("array_position(ARRAY['a', 'b', 'c'], 'b')"),
            None,
            None
        )
        .unwrap(),
        Value::Int32(2)
    );
    assert_eq!(
        eval_expr(&parse_expr("array_position(ARRAY[1, 2, 3], 5)"), None, None).unwrap(),
        Value::Null
    );
}

#[test]
fn test_array_cat() {
    assert_eq!(
        eval_expr(
            &parse_expr("array_cat(ARRAY[1, 2], ARRAY[3, 4])"),
            None,
            None
        )
        .unwrap(),
        Value::Array(vec![
            Value::Int32(1),
            Value::Int32(2),
            Value::Int32(3),
            Value::Int32(4)
        ])
    );
}

#[test]
fn test_array_append_prepend() {
    assert_eq!(
        eval_expr(&parse_expr("array_append(ARRAY[1, 2], 3)"), None, None).unwrap(),
        Value::Array(vec![Value::Int32(1), Value::Int32(2), Value::Int32(3)])
    );
    assert_eq!(
        eval_expr(&parse_expr("array_prepend(0, ARRAY[1, 2])"), None, None).unwrap(),
        Value::Array(vec![Value::Int32(0), Value::Int32(1), Value::Int32(2)])
    );
}

#[test]
fn test_cardinality() {
    assert_eq!(
        eval_expr(&parse_expr("cardinality(ARRAY[1, 2, 3, 4])"), None, None).unwrap(),
        Value::Int32(4)
    );
}

#[test]
fn test_json_cast() {
    assert_eq!(
        eval_expr(&parse_expr(r#"'{"a": 1}'::json ->> 'a'"#), None, None).unwrap(),
        Value::Text("1".to_string())
    );
}

#[test]
fn test_jsonb_cast() {
    assert_eq!(
        eval_expr(&parse_expr(r#"'{"b": 2}'::jsonb ->> 'b'"#), None, None).unwrap(),
        Value::Text("2".to_string())
    );
}

#[test]
fn test_json_comparison_blocked() {
    let json_val = Value::Json(r#"{"a":1}"#.to_string());
    let int_val = Value::Int32(1);
    assert!(compare_values(&json_val, &int_val).is_err());
}

#[test]
fn test_jsonb_comparison_blocked() {
    let jsonb_val = Value::Jsonb(r#"{"a":1}"#.to_string());
    let int_val = Value::Int32(1);
    assert!(compare_values(&jsonb_val, &int_val).is_err());
}

#[test]
fn test_json_contains_at_arrow() {
    assert_eq!(
        eval_expr(
            &parse_expr(r#"'{"a":1,"b":2}'::jsonb @> '{"a":1}'::jsonb"#),
            None,
            None
        )
        .unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(
        eval_expr(
            &parse_expr(r#"'{"a":1}'::jsonb @> '{"a":1,"b":2}'::jsonb"#),
            None,
            None
        )
        .unwrap(),
        Value::Boolean(false)
    );
    assert_eq!(
        eval_expr(
            &parse_expr(r#"'{"a":1}'::jsonb @> '{"a":1.0}'::jsonb"#),
            None,
            None
        )
        .unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(
        eval_expr(
            &parse_expr(r#"'[{"a":1,"b":2}]'::jsonb @> '[{"a":1}]'::jsonb"#),
            None,
            None
        )
        .unwrap(),
        Value::Boolean(true)
    );
}

#[test]
fn test_json_contained_by_arrow_at() {
    assert_eq!(
        eval_expr(
            &parse_expr(r#"'{"a":1}'::jsonb <@ '{"a":1,"b":2}'::jsonb"#),
            None,
            None
        )
        .unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(
        eval_expr(
            &parse_expr(r#"'{"a":1,"b":2}'::jsonb <@ '{"a":1}'::jsonb"#),
            None,
            None
        )
        .unwrap(),
        Value::Boolean(false)
    );
}

#[test]
fn test_parse_vector_literal() {
    let vec = parse_vector_literal("[1.0, 2.0, 3.0]").unwrap();
    assert_eq!(vec, vec![1.0, 2.0, 3.0]);

    let vec2 = parse_vector_literal("[1,2,3]").unwrap();
    assert_eq!(vec2, vec![1.0, 2.0, 3.0]);

    assert!(parse_vector_literal("not a vector").is_err());
    assert!(parse_vector_literal("[1, 2, abc]").is_err());
}

#[test]
fn test_l2_distance() {
    let v1 = vec![1.0, 0.0, 0.0];
    let v2 = vec![0.0, 1.0, 0.0];
    let dist = l2_distance(&v1, &v2).unwrap();
    assert!((dist - std::f64::consts::SQRT_2).abs() < 0.001);

    let v3 = vec![1.0, 2.0, 3.0];
    let v4 = vec![1.0, 2.0, 3.0];
    let dist2 = l2_distance(&v3, &v4).unwrap();
    assert!(dist2.abs() < 0.001); // Same vectors = 0 distance
}

#[test]
fn test_cosine_distance() {
    let v1 = vec![1.0, 0.0, 0.0];
    let v2 = vec![1.0, 0.0, 0.0];
    let dist = cosine_distance(&v1, &v2).unwrap();
    assert!(dist.abs() < 0.001); // Same vectors = 0 distance

    let v3 = vec![1.0, 0.0, 0.0];
    let v4 = vec![0.0, 1.0, 0.0];
    let dist2 = cosine_distance(&v3, &v4).unwrap();
    assert!((dist2 - 1.0).abs() < 0.001); // Orthogonal = max distance
}

#[test]
fn test_inner_product() {
    let v1 = vec![1.0, 2.0, 3.0];
    let v2 = vec![4.0, 5.0, 6.0];
    let prod = inner_product(&v1, &v2).unwrap();
    assert_eq!(prod, -(4.0 + 10.0 + 18.0)); // negative for ORDER BY
}

#[test]
fn test_vector_norm() {
    let v1 = vec![3.0, 4.0];
    let norm = vector_norm(&v1);
    assert_eq!(norm, 5.0); // 3-4-5 triangle

    let v2 = vec![1.0, 0.0, 0.0];
    let norm2 = vector_norm(&v2);
    assert_eq!(norm2, 1.0);
}

#[test]
fn test_extract_vector() {
    // Test with Vector value
    let vec_val = Value::Vector(vec![1.0, 2.0, 3.0]);
    let extracted = extract_vector(&vec_val).unwrap();
    assert_eq!(extracted, vec![1.0, 2.0, 3.0]);

    // Test with Array value
    let arr_val = Value::Array(vec![
        Value::Float64(1.0),
        Value::Float64(2.0),
        Value::Float64(3.0),
    ]);
    let extracted2 = extract_vector(&arr_val).unwrap();
    assert_eq!(extracted2, vec![1.0, 2.0, 3.0]);

    // Test with Int32 array
    let arr_int = Value::Array(vec![Value::Int32(1), Value::Int32(2), Value::Int32(3)]);
    let extracted3 = extract_vector(&arr_int).unwrap();
    assert_eq!(extracted3, vec![1.0, 2.0, 3.0]);

    // Test with Text value (NEW - for ORM compatibility)
    let text_val = Value::Text("[1.5, 2.5, 3.5]".to_string());
    let extracted4 = extract_vector(&text_val).unwrap();
    assert_eq!(extracted4, vec![1.5, 2.5, 3.5]);

    // Test with Text value with spaces
    let text_val2 = Value::Text(" [ 1.0 , 2.0 , 3.0 ] ".to_string());
    let extracted5 = extract_vector(&text_val2).unwrap();
    assert_eq!(extracted5, vec![1.0, 2.0, 3.0]);

    // Test with empty text vector
    let text_empty = Value::Text("[]".to_string());
    let extracted6 = extract_vector(&text_empty).unwrap();
    assert_eq!(extracted6, Vec::<f64>::new());
}

#[test]
fn test_format_width_and_identifier_quoting() {
    assert_eq!(
        eval_expr(&parse_expr("FORMAT('%10s', 'test')"), None, None).unwrap(),
        Value::Text("      test".to_string())
    );

    assert_eq!(
        eval_expr(&parse_expr("FORMAT('%I', 'column_name')"), None, None).unwrap(),
        Value::Text("column_name".to_string())
    );

    assert_eq!(
        eval_expr(&parse_expr("FORMAT('%I', 'column name')"), None, None).unwrap(),
        Value::Text("\"column name\"".to_string())
    );

    assert_eq!(
        eval_expr(&parse_expr("FORMAT('%L', 'value''s')"), None, None).unwrap(),
        Value::Text("'value''s'".to_string())
    );
}

#[test]
fn test_format_rejects_precision_like_postgres() {
    let err = eval_expr(&parse_expr("FORMAT('%.3s', 'hello')"), None, None).unwrap_err();
    assert!(
        err.to_string()
            .contains("unrecognized format() type specifier \".\""),
        "unexpected error: {}",
        err
    );
}

#[test]
fn test_quote_ident_and_pg_typeof_array() {
    assert_eq!(
        eval_expr(&parse_expr("QUOTE_IDENT('column')"), None, None).unwrap(),
        Value::Text("\"column\"".to_string())
    );

    assert_eq!(
        eval_expr(&parse_expr("PG_TYPEOF(ARRAY[1,2,3])"), None, None).unwrap(),
        Value::Text("integer[]".to_string())
    );
}

#[test]
fn test_pg_encoding_to_char_reports_utf8() {
    let expr = parse_expr("pg_encoding_to_char(6)");
    let val = eval_expr(&expr, None, None).unwrap();
    assert_eq!(val, Value::Text("UTF8".to_string()));
}

#[tokio::test]
async fn test_current_database_reads_task_local_context() {
    let expr = parse_expr("current_database()");
    let val = with_query_context(123, Arc::from("mydb"), async {
        eval_expr(&expr, None, None).unwrap()
    })
    .await;
    assert_eq!(val, Value::Text("mydb".to_string()));
}

#[tokio::test]
async fn test_current_timestamp_precision_truncates_to_second() {
    let txn = 1_700_000_001_234_i64;
    let stmt = txn + 1000; // statement is later; CURRENT_TIMESTAMP should use txn
    let expr = parse_expr("CURRENT_TIMESTAMP(0)");
    let val =
        statement_time::with_timestamps(stmt, txn, async { eval_expr(&expr, None, None).unwrap() })
            .await;
    assert_eq!(val, Value::Timestamp(1_700_000_001_000));
}

#[tokio::test]
async fn test_current_date_uses_transaction_timestamp() {
    let txn = 1_700_000_001_234_i64;
    let stmt = txn + 1000;
    let expr = parse_expr("CURRENT_DATE");
    let val =
        statement_time::with_timestamps(stmt, txn, async { eval_expr(&expr, None, None).unwrap() })
            .await;
    let expected_days = crate::types::date::timestamp_millis_to_date_days(txn).unwrap();
    assert_eq!(val, Value::Date(expected_days));
}

#[test]
fn test_current_date_reads_from_query_context() {
    use crate::sql::query_context::QueryContext;

    let stmt_ts = 1_700_000_099_000_i64;
    let txn_ts = 1_700_000_001_234_i64;
    let qc = QueryContext::new(1, Arc::from("db"), stmt_ts, txn_ts, Arc::from("UTC"));
    let expr = parse_expr("CURRENT_DATE");
    let val = eval_expr_with_query_ctx(&expr, None, None, Some(&qc)).unwrap();
    let expected_days = crate::types::date::timestamp_millis_to_date_days(txn_ts).unwrap();
    assert_eq!(val, Value::Date(expected_days));
}

#[tokio::test]
async fn test_age_single_arg_uses_transaction_timestamp() {
    // AGE(CURRENT_TIMESTAMP) should produce a zero interval when both
    // the argument and the implicit "current" reference resolve to the
    // same transaction timestamp — proving AGE reads from the task-local
    // rather than calling SystemTime::now().
    let txn = 1_700_000_001_000_i64;
    let stmt = txn + 5000;
    let expr = parse_expr("AGE(CURRENT_TIMESTAMP)");
    let val =
        statement_time::with_timestamps(stmt, txn, async { eval_expr(&expr, None, None).unwrap() })
            .await;
    match val {
        Value::Interval(iv) => {
            assert_eq!(iv.months, 0, "months should be 0");
            assert_eq!(iv.millis, 0, "millis should be 0");
        }
        other => panic!("expected Interval, got {:?}", other),
    }
}

#[tokio::test]
async fn test_now_precision_matches_current_timestamp_precision() {
    let txn = 1_700_000_001_234_i64;
    let stmt = txn + 1000;
    let expr = parse_expr("NOW(0) = CURRENT_TIMESTAMP(0)");
    let val =
        statement_time::with_timestamps(stmt, txn, async { eval_expr(&expr, None, None).unwrap() })
            .await;
    assert_eq!(val, Value::Boolean(true));
}

#[tokio::test]
async fn test_current_timestamp_equals_date_trunc_second_within_statement() {
    let txn = 1_700_000_001_234_i64;
    let stmt = txn + 1000;
    let expr = parse_expr("CURRENT_TIMESTAMP(0) = DATE_TRUNC('second', CURRENT_TIMESTAMP)");
    let val =
        statement_time::with_timestamps(stmt, txn, async { eval_expr(&expr, None, None).unwrap() })
            .await;
    assert_eq!(val, Value::Boolean(true));
}

#[test]
fn test_date_trunc_second_handles_negative_timestamps() {
    let expr = parse_expr("DATE_TRUNC('second', TIMESTAMP '1969-12-31 23:59:58.766')");
    let val = eval_expr(&expr, None, None).unwrap();
    assert_eq!(val, parse_timestamp_string("1969-12-31 23:59:58").unwrap());
}

#[test]
fn test_cast_timestamp_to_text_formats_timestamp() {
    let expr = parse_expr("TIMESTAMP '2024-01-15 10:30:00'::text");
    let val = eval_expr(&expr, None, None).unwrap();
    assert_eq!(val, Value::Text("2024-01-15 10:30:00".to_string()));
}

#[tokio::test]
async fn test_cast_timestamptz_to_text_includes_offset() {
    use std::sync::Arc;

    let expr = parse_expr("TIMESTAMPTZ '2024-01-15T10:00:00Z'::text");
    let val = crate::session_context::with_timezone(Arc::from("America/Los_Angeles"), async {
        eval_expr(&expr, None, None).unwrap()
    })
    .await;
    assert_eq!(val, Value::Text("2024-01-15 02:00:00-08".to_string()));
}

// --- QueryContext integration tests ---

#[test]
fn test_pg_backend_pid_reads_from_query_context() {
    use crate::sql::query_context::QueryContext;

    let qc = QueryContext::new(
        999,
        Arc::from("testdb"),
        1_700_000_000_000,
        1_700_000_000_000,
        Arc::from("UTC"),
    );
    let expr = parse_expr("pg_backend_pid()");
    let val = eval_expr_with_query_ctx(&expr, None, None, Some(&qc)).unwrap();
    assert_eq!(val, Value::Int32(999));
}

#[test]
fn test_current_database_reads_from_query_context() {
    use crate::sql::query_context::QueryContext;

    let qc = QueryContext::new(
        1,
        Arc::from("context_db"),
        1_700_000_000_000,
        1_700_000_000_000,
        Arc::from("UTC"),
    );
    let expr = parse_expr("current_database()");
    let val = eval_expr_with_query_ctx(&expr, None, None, Some(&qc)).unwrap();
    assert_eq!(val, Value::Text("context_db".to_string()));
}

#[test]
fn test_now_reads_transaction_timestamp_from_query_context() {
    use crate::sql::query_context::QueryContext;

    let stmt_ts = 1_700_000_099_000_i64;
    let txn_ts = 1_700_000_001_234_i64;
    let qc = QueryContext::new(1, Arc::from("db"), stmt_ts, txn_ts, Arc::from("UTC"));
    let expr = parse_expr("NOW(0)");
    let val = eval_expr_with_query_ctx(&expr, None, None, Some(&qc)).unwrap();
    // NOW() uses transaction time per PostgreSQL semantics
    assert_eq!(val, Value::Timestamp(1_700_000_001_000));
}

#[test]
fn test_current_timestamp_reads_transaction_timestamp_from_query_context() {
    use crate::sql::query_context::QueryContext;

    let stmt_ts = 1_700_000_099_000_i64;
    let txn_ts = 1_700_000_001_500_i64;
    let qc = QueryContext::new(1, Arc::from("db"), stmt_ts, txn_ts, Arc::from("UTC"));
    let expr = parse_expr("CURRENT_TIMESTAMP(3)");
    let val = eval_expr_with_query_ctx(&expr, None, None, Some(&qc)).unwrap();
    // CURRENT_TIMESTAMP uses transaction time per PostgreSQL semantics
    assert_eq!(val, Value::Timestamp(1_700_000_001_500));
}

#[tokio::test]
async fn test_query_context_overrides_task_local() {
    use crate::sql::query_context::QueryContext;

    let qc = QueryContext::new(
        777,
        Arc::from("qc_db"),
        1_600_000_000_000,
        1_600_000_000_000,
        Arc::from("UTC"),
    );

    let pid_expr = parse_expr("pg_backend_pid()");
    let db_expr = parse_expr("current_database()");

    let (pid, db) = with_query_context(123, Arc::from("task_local_db"), async {
        let pid = eval_expr_with_query_ctx(&pid_expr, None, None, Some(&qc)).unwrap();
        let db = eval_expr_with_query_ctx(&db_expr, None, None, Some(&qc)).unwrap();
        (pid, db)
    })
    .await;

    assert_eq!(pid, Value::Int32(777));
    assert_eq!(db, Value::Text("qc_db".to_string()));
}

#[test]
fn test_transaction_timestamp_reads_from_query_context() {
    use crate::sql::query_context::QueryContext;

    let stmt_ts = 1_700_000_001_000_i64;
    let txn_ts = 1_700_000_000_000_i64;
    let qc = QueryContext::new(1, Arc::from("db"), stmt_ts, txn_ts, Arc::from("UTC"));
    let expr = parse_expr("TRANSACTION_TIMESTAMP(0)");
    let val = eval_expr_with_query_ctx(&expr, None, None, Some(&qc)).unwrap();
    assert_eq!(val, Value::Timestamp(txn_ts));
}

#[tokio::test]
async fn test_transaction_timestamp_reads_task_local_without_query_ctx() {
    let stmt = 1_700_000_001_000_i64;
    let txn = 1_700_000_000_000_i64;
    let expr = parse_expr("TRANSACTION_TIMESTAMP(0)");
    let val =
        statement_time::with_timestamps(stmt, txn, async { eval_expr(&expr, None, None).unwrap() })
            .await;
    assert_eq!(
        val,
        Value::Timestamp(txn),
        "TRANSACTION_TIMESTAMP() should read TRANSACTION_TIMESTAMP_MILLIS task-local even without QueryContext"
    );
}

#[tokio::test]
async fn test_transaction_timestamp_differs_from_statement_timestamp() {
    let stmt = 1_700_000_001_000_i64;
    let txn = 1_700_000_000_000_i64;
    let txn_expr = parse_expr("TRANSACTION_TIMESTAMP(0)");
    let stmt_expr = parse_expr("STATEMENT_TIMESTAMP(0)");
    let (txn_val, stmt_val) = statement_time::with_timestamps(stmt, txn, async {
        (
            eval_expr(&txn_expr, None, None).unwrap(),
            eval_expr(&stmt_expr, None, None).unwrap(),
        )
    })
    .await;
    assert_eq!(txn_val, Value::Timestamp(txn));
    assert_eq!(stmt_val, Value::Timestamp(stmt));
    assert_ne!(
        txn_val, stmt_val,
        "transaction and statement timestamps should differ in explicit transaction"
    );
}

#[tokio::test]
async fn test_now_equals_transaction_timestamp_not_statement_timestamp() {
    // Per PostgreSQL: NOW() = CURRENT_TIMESTAMP = TRANSACTION_TIMESTAMP()
    // and all differ from STATEMENT_TIMESTAMP() inside an explicit transaction.
    let stmt = 1_700_000_005_000_i64;
    let txn = 1_700_000_000_000_i64;
    let (now_val, ct_val, tt_val, st_val) = statement_time::with_timestamps(stmt, txn, async {
        (
            eval_expr(&parse_expr("NOW(0)"), None, None).unwrap(),
            eval_expr(&parse_expr("CURRENT_TIMESTAMP(0)"), None, None).unwrap(),
            eval_expr(&parse_expr("TRANSACTION_TIMESTAMP(0)"), None, None).unwrap(),
            eval_expr(&parse_expr("STATEMENT_TIMESTAMP(0)"), None, None).unwrap(),
        )
    })
    .await;
    // NOW, CURRENT_TIMESTAMP, TRANSACTION_TIMESTAMP all return transaction time
    assert_eq!(now_val, Value::Timestamp(txn));
    assert_eq!(ct_val, Value::Timestamp(txn));
    assert_eq!(tt_val, Value::Timestamp(txn));
    // STATEMENT_TIMESTAMP returns statement time
    assert_eq!(st_val, Value::Timestamp(stmt));
}

#[test]
fn test_eval_expr_without_query_context_falls_back() {
    let expr = parse_expr("pg_backend_pid()");
    set_connection_id(42);
    let val = eval_expr_with_query_ctx(&expr, None, None, None).unwrap();
    assert_eq!(val, Value::Int32(42));
}

#[test]
fn test_like_single_byte_escape_accepted() {
    // ASCII (1 byte) should work fine
    assert_eq!(
        eval_expr(&parse_expr("'a_b' LIKE 'a\\_b' ESCAPE '\\'"), None, None).unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(
        eval_expr(&parse_expr("'a%b' LIKE 'a\\%b' ESCAPE '\\'"), None, None).unwrap(),
        Value::Boolean(true)
    );
}

// ── Error path tests for #657 (error masking elimination) ──────────
//
// After #657, compare_values() errors propagate via ? instead of being
// silently swallowed by unwrap_or(0). These tests verify that:
//   1. Truly incomparable types (jsonb, vector) produce errors that
//      propagate through GREATEST/LEAST/IN/BETWEEN/CASE.
//   2. Cross-type comparisons that CAN be coerced (Text vs Int) still work.
//   3. Parse failures produce errors instead of silent zero.

#[test]
fn test_greatest_jsonb_vs_int_errors() {
    // jsonb has no ordering operator — GREATEST must propagate the error
    let err = eval_expr(
        &parse_expr("GREATEST('{\"a\":1}'::jsonb, '{\"b\":2}'::jsonb)"),
        None,
        None,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("ordering operator for type jsonb"),
        "expected jsonb ordering error, got: {}",
        err
    );
}

#[test]
fn test_least_jsonb_errors() {
    let err = eval_expr(
        &parse_expr("LEAST('{\"a\":1}'::jsonb, '{\"b\":2}'::jsonb)"),
        None,
        None,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("ordering operator for type jsonb"),
        "expected jsonb ordering error, got: {}",
        err
    );
}

#[test]
fn test_nullif_numeric_vs_non_numeric_text_errors() {
    // Numeric compared to non-numeric text should error
    let err = eval_expr(&parse_expr("NULLIF(1.5::numeric, 'abc')"), None, None).unwrap_err();
    assert!(
        err.to_string().contains("Cannot compare numeric"),
        "expected numeric comparison error, got: {}",
        err
    );
}

#[test]
fn test_in_list_jsonb_errors() {
    let err = eval_expr(
        &parse_expr("'{\"a\":1}'::jsonb IN ('{\"b\":2}'::jsonb)"),
        None,
        None,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("ordering operator for type jsonb")
            || err
                .to_string()
                .contains("comparison function for type json"),
        "expected jsonb comparison error, got: {}",
        err
    );
}

#[test]
fn test_between_jsonb_errors() {
    let err = eval_expr(
        &parse_expr("'{\"a\":1}'::jsonb BETWEEN '{\"a\":0}'::jsonb AND '{\"a\":9}'::jsonb"),
        None,
        None,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("ordering operator for type jsonb"),
        "expected jsonb comparison error, got: {}",
        err
    );
}

#[test]
fn test_case_when_simple_jsonb_errors() {
    let err = eval_expr(
        &parse_expr("CASE '{\"a\":1}'::jsonb WHEN '{\"a\":2}'::jsonb THEN 'match' END"),
        None,
        None,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("ordering operator for type jsonb"),
        "expected jsonb comparison error, got: {}",
        err
    );
}

#[test]
fn test_interval_non_numeric_string_errors() {
    let err = eval_expr(&parse_expr("INTERVAL 'abc' DAY"), None, None).unwrap_err();
    assert!(
        err.to_string().contains("invalid input syntax"),
        "expected parse error, got: {}",
        err
    );
}

#[test]
fn test_compare_values_incompatible_types_error() {
    // Json/Jsonb cannot be compared to scalars
    assert!(compare_values(&Value::Json("{}".to_string()), &Value::Int32(1)).is_err());
    assert!(compare_values(&Value::Jsonb("{}".to_string()), &Value::Int32(1)).is_err());
    // Vector cannot be compared
    assert!(compare_values(&Value::Vector(vec![1.0]), &Value::Int32(1)).is_err());
}

#[test]
fn test_cross_type_coercion_still_works() {
    // Text vs Int32 uses string coercion — must still work (not error)
    assert!(eval_expr(&parse_expr("GREATEST('abc', 123)"), None, None).is_ok());
    assert!(eval_expr(&parse_expr("LEAST('abc', 123)"), None, None).is_ok());
    assert!(eval_expr(&parse_expr("NULLIF(123, 'abc')"), None, None).is_ok());
    assert!(eval_expr(&parse_expr("1 IN ('a', 'b')"), None, None).is_ok());
    assert!(eval_expr(&parse_expr("1 BETWEEN 'a' AND 'z'"), None, None).is_ok());
}

#[test]
fn test_numeric_decimal_overflow_errors() {
    // Decimal that overflows i64 in modulo operation
    let err = eval_expr(
        &parse_expr("99999999999999999999999::numeric % 1"),
        None,
        None,
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("numeric value out of range")
            || msg.contains("out of range")
            || msg.contains("number too large"),
        "expected overflow error, got: {}",
        msg
    );
}
