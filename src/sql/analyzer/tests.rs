//! Analyzer unit tests.
//!
//! Uses `MockCatalog` and `sqlparser` to test expression and query analysis.

use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

use crate::sql::analyzer::catalog::MockCatalog;
use crate::sql::analyzer::scope::Scope;
use crate::sql::analyzer::types::*;
use crate::sql::analyzer::{Analyzer, AnalyzerError};
use crate::types::DataType;

/// Extract the `AnalyzedSelect` from a query, panicking if it's not a SELECT.
fn expect_select(query: &AnalyzedQuery) -> &AnalyzedSelect {
    match &query.body {
        AnalyzedQueryBody::Select(s) => s,
        other => panic!("expected Select, got {:?}", std::mem::discriminant(other)),
    }
}

fn parse_expr(sql: &str) -> sqlparser::ast::Expr {
    let dialect = PostgreSqlDialect {};
    let stmts = Parser::parse_sql(&dialect, &format!("SELECT {}", sql)).unwrap();
    match &stmts[0] {
        sqlparser::ast::Statement::Query(q) => match &*q.body {
            sqlparser::ast::SetExpr::Select(s) => match &s.projection[0] {
                sqlparser::ast::SelectItem::UnnamedExpr(e) => e.clone(),
                sqlparser::ast::SelectItem::ExprWithAlias { expr, .. } => expr.clone(),
                _ => panic!("unexpected select item"),
            },
            _ => panic!("unexpected set expr"),
        },
        _ => panic!("unexpected statement"),
    }
}

fn parse_query(sql: &str) -> sqlparser::ast::Query {
    let dialect = PostgreSqlDialect {};
    let stmts = Parser::parse_sql(&dialect, sql).unwrap();
    match stmts.into_iter().next().unwrap() {
        sqlparser::ast::Statement::Query(q) => *q,
        _ => panic!("expected query"),
    }
}

fn test_catalog() -> MockCatalog {
    MockCatalog::builder()
        .table(
            "users",
            vec![
                ("id", DataType::Int32, false),
                ("name", DataType::Text, true),
                ("age", DataType::Int32, true),
                ("email", DataType::Text, true),
                ("active", DataType::Boolean, true),
                ("score", DataType::Float64, true),
                ("created_at", DataType::Timestamp, true),
            ],
        )
        .table(
            "orders",
            vec![
                ("order_id", DataType::Int32, false),
                ("user_id", DataType::Int32, false),
                (
                    "amount",
                    DataType::Numeric {
                        precision: None,
                        scale: None,
                    },
                    true,
                ),
                ("status", DataType::Text, true),
            ],
        )
        .table(
            "products",
            vec![
                ("product_id", DataType::Int32, false),
                ("name", DataType::Text, false),
                ("price", DataType::Float64, false),
                ("tags", DataType::Array(Box::new(DataType::Text)), true),
            ],
        )
        .build()
}

fn analyze_expr_with_users(sql: &str) -> Result<TypedExpr, AnalyzerError> {
    let catalog = test_catalog();
    let mut scope = Scope::new();
    scope.allow_aggregates = true;
    scope.allow_windows = true;
    scope.add_table(
        "users",
        &[
            ("id".to_string(), DataType::Int32, false),
            ("name".to_string(), DataType::Text, true),
            ("age".to_string(), DataType::Int32, true),
            ("email".to_string(), DataType::Text, true),
            ("active".to_string(), DataType::Boolean, true),
            ("score".to_string(), DataType::Float64, true),
            ("created_at".to_string(), DataType::Timestamp, true),
        ],
    );
    Analyzer::analyze_expr_with_scope(&catalog, scope, &parse_expr(sql))
}

/// Like analyze_expr_with_users, but with aggregates disallowed (simulates WHERE context).
// Test infrastructure -- will be wired up when analyzer tests expand.
#[allow(dead_code)]
fn analyze_expr_no_aggregates(sql: &str) -> Result<TypedExpr, AnalyzerError> {
    let catalog = test_catalog();
    let mut scope = Scope::new();
    scope.add_table(
        "users",
        &[
            ("id".to_string(), DataType::Int32, false),
            ("name".to_string(), DataType::Text, true),
            ("age".to_string(), DataType::Int32, true),
            ("email".to_string(), DataType::Text, true),
            ("active".to_string(), DataType::Boolean, true),
            ("score".to_string(), DataType::Float64, true),
            ("created_at".to_string(), DataType::Timestamp, true),
        ],
    );
    Analyzer::analyze_expr_with_scope(&catalog, scope, &parse_expr(sql))
}

// ── Constant analysis ───────────────────────────────────────

#[test]
fn analyze_integer_literal() {
    let expr = analyze_expr_with_users("42").unwrap();
    assert_eq!(expr.data_type, DataType::Int32);
    assert!(matches!(
        expr.kind,
        TypedExprKind::Constant(crate::types::Value::Int32(42))
    ));
}

#[test]
fn analyze_bigint_literal() {
    let expr = analyze_expr_with_users("9999999999").unwrap();
    assert_eq!(expr.data_type, DataType::Int64);
}

#[test]
fn analyze_float_literal() {
    let expr = analyze_expr_with_users("3.14e2").unwrap();
    assert_eq!(expr.data_type, DataType::Float64);
}

#[test]
fn analyze_numeric_literal() {
    let expr = analyze_expr_with_users("3.14").unwrap();
    assert!(matches!(expr.data_type, DataType::Numeric { .. }));
}

#[test]
fn analyze_string_literal() {
    let expr = analyze_expr_with_users("'hello'").unwrap();
    assert_eq!(expr.data_type, DataType::Text);
}

#[test]
fn analyze_boolean_literal() {
    let expr = analyze_expr_with_users("true").unwrap();
    assert_eq!(expr.data_type, DataType::Boolean);
}

#[test]
fn analyze_null_literal() {
    let expr = analyze_expr_with_users("NULL").unwrap();
    assert_eq!(expr.data_type, DataType::Text); // PG default
    assert!(expr.is_null_constant());
}

// ── Column reference ────────────────────────────────────────

#[test]
fn analyze_column_ref() {
    let expr = analyze_expr_with_users("id").unwrap();
    assert_eq!(expr.data_type, DataType::Int32);
    match &expr.kind {
        TypedExprKind::ColumnRef {
            scope_depth,
            column_index,
            column_name,
        } => {
            assert_eq!(*scope_depth, 0);
            assert_eq!(*column_index, 0);
            assert_eq!(column_name, "id");
        }
        _ => panic!("expected ColumnRef"),
    }
}

#[test]
fn analyze_qualified_column_ref() {
    let expr = analyze_expr_with_users("users.name").unwrap();
    assert_eq!(expr.data_type, DataType::Text);
    match &expr.kind {
        TypedExprKind::ColumnRef { column_index, .. } => {
            assert_eq!(*column_index, 1);
        }
        _ => panic!("expected ColumnRef"),
    }
}

#[test]
fn analyze_column_not_found() {
    let err = analyze_expr_with_users("nonexistent").unwrap_err();
    assert!(matches!(err, AnalyzerError::ColumnNotFound { .. }));
}

// ── Binary operators ────────────────────────────────────────

#[test]
fn analyze_arithmetic() {
    let expr = analyze_expr_with_users("age + 1").unwrap();
    assert_eq!(expr.data_type, DataType::Int32);
    assert!(matches!(
        expr.kind,
        TypedExprKind::BinaryOp {
            op: BinaryOp::Add,
            ..
        }
    ));
}

#[test]
fn analyze_comparison() {
    let expr = analyze_expr_with_users("age > 18").unwrap();
    assert_eq!(expr.data_type, DataType::Boolean);
}

#[test]
fn analyze_string_concat() {
    let expr = analyze_expr_with_users("name || ' ' || email").unwrap();
    assert_eq!(expr.data_type, DataType::Text);
}

#[test]
fn analyze_logical() {
    let expr = analyze_expr_with_users("age > 18 AND active").unwrap();
    assert_eq!(expr.data_type, DataType::Boolean);
}

// ── Unary operators ─────────────────────────────────────────

#[test]
fn analyze_not() {
    let expr = analyze_expr_with_users("NOT active").unwrap();
    assert_eq!(expr.data_type, DataType::Boolean);
    assert!(matches!(
        expr.kind,
        TypedExprKind::UnaryOp {
            op: UnaryOp::Not,
            ..
        }
    ));
}

#[test]
fn analyze_negation() {
    let expr = analyze_expr_with_users("-age").unwrap();
    assert_eq!(expr.data_type, DataType::Int32);
}

// ── Cast ────────────────────────────────────────────────────

#[test]
fn analyze_cast() {
    let expr = analyze_expr_with_users("CAST(age AS TEXT)").unwrap();
    assert_eq!(expr.data_type, DataType::Text);
    assert!(matches!(expr.kind, TypedExprKind::Cast { .. }));
}

#[test]
fn analyze_double_colon_cast() {
    let expr = analyze_expr_with_users("age::text").unwrap();
    assert_eq!(expr.data_type, DataType::Text);
}

// ── IS tests ────────────────────────────────────────────────

#[test]
fn analyze_is_null() {
    let expr = analyze_expr_with_users("name IS NULL").unwrap();
    assert_eq!(expr.data_type, DataType::Boolean);
    assert!(matches!(
        expr.kind,
        TypedExprKind::IsTest {
            test: IsTestKind::Null,
            negated: false,
            ..
        }
    ));
}

#[test]
fn analyze_is_not_null() {
    let expr = analyze_expr_with_users("name IS NOT NULL").unwrap();
    assert_eq!(expr.data_type, DataType::Boolean);
    assert!(matches!(
        expr.kind,
        TypedExprKind::IsTest {
            test: IsTestKind::Null,
            negated: true,
            ..
        }
    ));
}

// ── BETWEEN ─────────────────────────────────────────────────

#[test]
fn analyze_between() {
    let expr = analyze_expr_with_users("age BETWEEN 18 AND 65").unwrap();
    assert_eq!(expr.data_type, DataType::Boolean);
    assert!(matches!(
        expr.kind,
        TypedExprKind::Between { negated: false, .. }
    ));
}

#[test]
fn analyze_between_coerces_types() {
    // age (Int32) BETWEEN Int64 AND Int32 → all coerced to Int64
    let expr = analyze_expr_with_users("age BETWEEN 9999999999 AND 100").unwrap();
    assert_eq!(expr.data_type, DataType::Boolean);
    match &expr.kind {
        TypedExprKind::Between {
            expr, low, high, ..
        } => {
            // All three should be Int64
            assert_eq!(expr.data_type, DataType::Int64);
            assert_eq!(low.data_type, DataType::Int64);
            assert_eq!(high.data_type, DataType::Int64);
            // expr and high (originally Int32) should have implicit casts
            assert!(matches!(&expr.kind, TypedExprKind::Cast { .. }));
            assert!(matches!(&high.kind, TypedExprKind::Cast { .. }));
        }
        _ => panic!("expected Between"),
    }
}

// ── IN list ─────────────────────────────────────────────────

#[test]
fn analyze_in_list() {
    let expr = analyze_expr_with_users("age IN (18, 21, 30)").unwrap();
    assert_eq!(expr.data_type, DataType::Boolean);
    match &expr.kind {
        TypedExprKind::InList { list, negated, .. } => {
            assert_eq!(list.len(), 3);
            assert!(!negated);
        }
        _ => panic!("expected InList"),
    }
}

#[test]
fn analyze_in_list_coerces_types() {
    // age (Int32) IN (Int64, Int32) → all coerced to Int64
    let expr = analyze_expr_with_users("age IN (9999999999, 30)").unwrap();
    assert_eq!(expr.data_type, DataType::Boolean);
    match &expr.kind {
        TypedExprKind::InList { expr, list, .. } => {
            // expr (Int32) should be cast to Int64
            assert_eq!(expr.data_type, DataType::Int64);
            assert!(matches!(&expr.kind, TypedExprKind::Cast { .. }));
            // Second list element (Int32) should also be cast to Int64
            assert_eq!(list[1].data_type, DataType::Int64);
        }
        _ => panic!("expected InList"),
    }
}

// ── LIKE ────────────────────────────────────────────────────

#[test]
fn analyze_like() {
    let expr = analyze_expr_with_users("name LIKE '%john%'").unwrap();
    assert_eq!(expr.data_type, DataType::Boolean);
    assert!(matches!(
        expr.kind,
        TypedExprKind::Like {
            case_insensitive: false,
            negated: false,
            ..
        }
    ));
}

#[test]
fn analyze_ilike() {
    let expr = analyze_expr_with_users("name ILIKE '%john%'").unwrap();
    assert_eq!(expr.data_type, DataType::Boolean);
    assert!(matches!(
        expr.kind,
        TypedExprKind::Like {
            case_insensitive: true,
            ..
        }
    ));
}

// ── CASE ────────────────────────────────────────────────────

#[test]
fn analyze_case() {
    let expr = analyze_expr_with_users("CASE WHEN age > 18 THEN 'adult' ELSE 'minor' END").unwrap();
    assert_eq!(expr.data_type, DataType::Text);
    match &expr.kind {
        TypedExprKind::Case {
            when_clauses,
            else_result,
            ..
        } => {
            assert_eq!(when_clauses.len(), 1);
            assert!(else_result.is_some());
        }
        _ => panic!("expected Case"),
    }
}

// ── Functions ───────────────────────────────────────────────

#[test]
fn analyze_scalar_function() {
    let expr = analyze_expr_with_users("UPPER(name)").unwrap();
    assert_eq!(expr.data_type, DataType::Text);
    match &expr.kind {
        TypedExprKind::FunctionCall { func, args, .. } => {
            assert_eq!(func.name, "UPPER");
            assert_eq!(args.len(), 1);
            assert!(matches!(func.kind, FunctionKind::Builtin));
        }
        _ => panic!("expected FunctionCall"),
    }
}

#[test]
fn analyze_aggregate_function() {
    let expr = analyze_expr_with_users("COUNT(id)").unwrap();
    assert_eq!(expr.data_type, DataType::Int64);
    assert!(matches!(expr.kind, TypedExprKind::AggregateCall { .. }));
}

#[test]
fn analyze_count_star() {
    let expr = analyze_expr_with_users("COUNT(*)").unwrap();
    assert_eq!(expr.data_type, DataType::Int64);
}

// ── Syntax sugar normalization ──────────────────────────────

#[test]
fn analyze_substring_normalized() {
    let expr = analyze_expr_with_users("SUBSTRING(name FROM 1 FOR 3)").unwrap();
    assert_eq!(expr.data_type, DataType::Text);
    match &expr.kind {
        TypedExprKind::FunctionCall { func, args, .. } => {
            assert_eq!(func.name, "SUBSTRING");
            assert_eq!(args.len(), 3);
        }
        _ => panic!("expected FunctionCall (normalized from SUBSTRING syntax)"),
    }
}

#[test]
fn analyze_trim_normalized() {
    let expr = analyze_expr_with_users("TRIM(BOTH ' ' FROM name)").unwrap();
    assert_eq!(expr.data_type, DataType::Text);
    match &expr.kind {
        TypedExprKind::FunctionCall { func, .. } => {
            assert_eq!(func.name, "BTRIM");
        }
        _ => panic!("expected FunctionCall (normalized from TRIM syntax)"),
    }
}

#[test]
fn analyze_extract_normalized() {
    let expr = analyze_expr_with_users("EXTRACT(YEAR FROM created_at)").unwrap();
    assert_eq!(expr.data_type, DataType::Float64);
    match &expr.kind {
        TypedExprKind::FunctionCall { func, args, .. } => {
            assert_eq!(func.name, "DATE_PART");
            assert_eq!(args.len(), 2);
        }
        _ => panic!("expected FunctionCall (normalized from EXTRACT syntax)"),
    }
}

// ── Typed literals ──────────────────────────────────────────

#[test]
fn analyze_date_literal() {
    let expr = analyze_expr_with_users("DATE '2024-01-15'").unwrap();
    assert_eq!(expr.data_type, DataType::Date);
    assert!(matches!(
        expr.kind,
        TypedExprKind::Constant(crate::types::Value::Date(_))
    ));
}

// ── NULL type coercion ──────────────────────────────────────

#[test]
fn analyze_null_coercion_in_comparison() {
    let expr = analyze_expr_with_users("age = NULL").unwrap();
    assert_eq!(expr.data_type, DataType::Boolean);
    // The NULL should have been typed as Int32 (same as 'age')
    match &expr.kind {
        TypedExprKind::BinaryOp { right, .. } => {
            assert_eq!(right.data_type, DataType::Int32);
            assert!(right.is_null_constant());
        }
        _ => panic!("expected BinaryOp"),
    }
}

// ── Query-level analysis ────────────────────────────────────

#[test]
fn analyze_simple_select() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("SELECT id, name FROM users WHERE age > 18");
    let result = analyzer.analyze_query(&query).unwrap();

    assert_eq!(result.output_schema.len(), 2);
    assert_eq!(result.output_schema[0], ("id".to_string(), DataType::Int32));
    assert_eq!(
        result.output_schema[1],
        ("name".to_string(), DataType::Text)
    );
    assert!(expect_select(&result).where_clause.is_some());
}

#[test]
fn analyze_select_star() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("SELECT * FROM users");
    let result = analyzer.analyze_query(&query).unwrap();

    assert_eq!(result.output_schema.len(), 7); // all users columns
    assert_eq!(result.output_schema[0].0, "id");
    assert_eq!(result.output_schema[6].0, "created_at");
}

#[test]
fn analyze_select_with_alias() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("SELECT id AS user_id, name AS user_name FROM users");
    let result = analyzer.analyze_query(&query).unwrap();

    assert_eq!(result.output_schema[0].0, "user_id");
    assert_eq!(result.output_schema[1].0, "user_name");
}

#[test]
fn analyze_join() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query(
        "SELECT users.id, orders.amount FROM users JOIN orders ON users.id = orders.user_id",
    );
    let result = analyzer.analyze_query(&query).unwrap();

    assert_eq!(result.output_schema.len(), 2);
    assert_eq!(result.output_schema[0].1, DataType::Int32);
    assert!(matches!(
        result.output_schema[1].1,
        DataType::Numeric { .. }
    ));
}

#[test]
fn analyze_table_not_found() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("SELECT * FROM nonexistent");
    let err = analyzer.analyze_query(&query).unwrap_err();
    assert!(matches!(err, AnalyzerError::TableNotFound(_)));
}

#[test]
fn analyze_current_user_in_from_without_parentheses() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("SELECT * FROM CURRENT_USER AS t(u)");
    let result = analyzer.analyze_query(&query).unwrap();

    let select = expect_select(&result);
    assert_eq!(select.from.len(), 1);
    match &select.from[0].kind {
        AnalyzedTableRefKind::Function {
            func,
            args,
            output_columns,
        } => {
            assert_eq!(func.name, "CURRENT_USER");
            assert!(args.is_empty());
            assert_eq!(output_columns, &[("u".to_string(), DataType::Text)]);
        }
        other => panic!("expected Function table ref, got {:?}", other),
    }
}

#[test]
fn analyze_group_by() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("SELECT age, COUNT(id) FROM users GROUP BY age");
    let result = analyzer.analyze_query(&query).unwrap();

    assert_eq!(expect_select(&result).group_by.len(), 1);
    assert_eq!(result.output_schema.len(), 2);
}

#[test]
fn analyze_order_by() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("SELECT id, name FROM users ORDER BY age DESC");
    let result = analyzer.analyze_query(&query).unwrap();

    assert_eq!(result.order_by.len(), 1);
    assert!(!result.order_by[0].asc);
}

#[test]
fn analyze_distinct() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("SELECT DISTINCT name FROM users");
    let result = analyzer.analyze_query(&query).unwrap();

    assert!(matches!(
        expect_select(&result).distinct,
        AnalyzedDistinct::Distinct
    ));
}

#[test]
fn analyze_limit_offset() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("SELECT id FROM users LIMIT 10 OFFSET 5");
    let result = analyzer.analyze_query(&query).unwrap();

    assert!(result.limit.is_some());
    assert!(result.offset.is_some());
}

#[test]
fn analyze_subquery_in_where() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query(
        "SELECT id FROM users WHERE id IN (SELECT user_id FROM orders WHERE amount > 100)",
    );
    let result = analyzer.analyze_query(&query).unwrap();

    // The WHERE clause should contain an InSubquery
    assert!(expect_select(&result).where_clause.is_some());
}

#[test]
fn analyze_union() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query =
        parse_query("SELECT id, name FROM users UNION ALL SELECT order_id, status FROM orders");
    let result = analyzer.analyze_query(&query).unwrap();

    match &result.body {
        AnalyzedQueryBody::SetOperation { op, all, .. } => {
            assert_eq!(*op, SetOpKind::Union);
            assert!(*all);
        }
        _ => panic!("expected SetOperation"),
    }
}

// ── Implicit cast insertion ─────────────────────────────────

#[test]
fn analyze_implicit_cast_int32_plus_int64() {
    // age (Int32) + 9999999999 (Int64) → left gets implicit cast to Int64
    let expr = analyze_expr_with_users("age + 9999999999").unwrap();
    assert_eq!(expr.data_type, DataType::Int64);
    match &expr.kind {
        TypedExprKind::BinaryOp { left, right, .. } => {
            // Left should be wrapped in an implicit Cast Int32 → Int64
            assert!(matches!(
                &left.kind,
                TypedExprKind::Cast {
                    cast_context: crate::sql::types::CastContext::Implicit,
                    ..
                }
            ));
            assert_eq!(left.data_type, DataType::Int64);
            // Right is already Int64, no cast
            assert_eq!(right.data_type, DataType::Int64);
        }
        _ => panic!("expected BinaryOp"),
    }
}

#[test]
fn analyze_implicit_cast_int_eq_float() {
    // age (Int32) = 3.14 (Numeric) → Int32 cast to Numeric
    let expr = analyze_expr_with_users("age = 3.14").unwrap();
    assert_eq!(expr.data_type, DataType::Boolean);
    match &expr.kind {
        TypedExprKind::BinaryOp { left, .. } => {
            // Int32 operand should be implicitly cast to Numeric
            assert!(matches!(
                &left.kind,
                TypedExprKind::Cast {
                    cast_context: crate::sql::types::CastContext::Implicit,
                    ..
                }
            ));
        }
        _ => panic!("expected BinaryOp"),
    }
}

#[test]
fn analyze_no_cast_when_types_match() {
    // age (Int32) + 1 (Int32) → no cast needed
    let expr = analyze_expr_with_users("age + 1").unwrap();
    match &expr.kind {
        TypedExprKind::BinaryOp { left, right, .. } => {
            // Neither side should have a Cast wrapper
            assert!(matches!(left.kind, TypedExprKind::ColumnRef { .. }));
            assert!(matches!(right.kind, TypedExprKind::Constant(_)));
        }
        _ => panic!("expected BinaryOp"),
    }
}

// ── Unknown function passthrough ─────────────────────────────

#[test]
fn analyze_unknown_function_passthrough() {
    // Unknown functions are treated as opaque calls returning Text (passthrough for
    // pg-specific functions handled at runtime by eval_expr).
    let result = analyze_expr_with_users("totally_unknown_func(1)").unwrap();
    assert_eq!(result.data_type, DataType::Text);
    assert!(matches!(result.kind, TypedExprKind::FunctionCall { .. }));
}

#[test]
fn analyze_array_subscript_on_non_array_errors() {
    // score (Float64) is not an array
    let err = analyze_expr_with_users("score[1]").unwrap_err();
    assert!(matches!(err, AnalyzerError::OperatorTypeMismatch { .. }));
}

// ── Boolean context validation ───────────────────────────────

#[test]
fn analyze_where_must_be_boolean() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("SELECT id FROM users WHERE name");
    let err = analyzer.analyze_query(&query).unwrap_err();
    assert!(
        matches!(err, AnalyzerError::TypeMismatch { ref context, .. } if context == "WHERE clause")
    );
}

#[test]
fn analyze_having_must_be_boolean() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("SELECT age FROM users GROUP BY age HAVING age");
    let err = analyzer.analyze_query(&query).unwrap_err();
    assert!(
        matches!(err, AnalyzerError::TypeMismatch { ref context, .. } if context == "HAVING clause")
    );
}

#[test]
fn analyze_join_on_must_be_boolean() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("SELECT * FROM users JOIN orders ON users.name");
    let err = analyzer.analyze_query(&query).unwrap_err();
    assert!(
        matches!(err, AnalyzerError::TypeMismatch { ref context, .. } if context == "JOIN ON clause")
    );
}

#[test]
fn analyze_join_using_unifies_types() {
    // Table a has id:Int32, table b has id:Int64 → USING(id) should unify to Int64
    let catalog = MockCatalog::builder()
        .table(
            "a",
            vec![("id", DataType::Int32, false), ("x", DataType::Text, true)],
        )
        .table(
            "b",
            vec![("id", DataType::Int64, false), ("y", DataType::Text, true)],
        )
        .build();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("SELECT * FROM a JOIN b USING (id)");
    let result = analyzer.analyze_query(&query).unwrap();
    // The USING column should be unified to Int64
    let select = expect_select(&result);
    if let AnalyzedTableRefKind::Join { condition, .. } = &select.from[0].kind {
        if let JoinCondition::Using(cols) = condition {
            assert_eq!(cols[0].data_type, DataType::Int64);
        } else {
            panic!("expected USING condition");
        }
    } else {
        panic!("expected Join table ref");
    }
}

#[test]
fn analyze_join_using_incompatible_types_rejected() {
    // Table a has id:Int32, table b has id:Boolean → USING(id) should fail
    let catalog = MockCatalog::builder()
        .table("a", vec![("id", DataType::Int32, false)])
        .table("b", vec![("id", DataType::Boolean, false)])
        .build();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("SELECT * FROM a JOIN b USING (id)");
    let err = analyzer.analyze_query(&query).unwrap_err();
    assert!(matches!(err, AnalyzerError::OperatorTypeMismatch { .. }));
}

#[test]
fn analyze_case_when_condition_must_be_boolean() {
    // Searched CASE: conditions must be boolean
    let err = analyze_expr_with_users("CASE WHEN name THEN 'yes' ELSE 'no' END").unwrap_err();
    assert!(
        matches!(err, AnalyzerError::TypeMismatch { ref context, .. } if context == "CASE WHEN condition")
    );
}

// ── Aggregate context validation ────────────────────────────

#[test]
fn analyze_aggregate_in_where_rejected() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("SELECT id FROM users WHERE COUNT(id) > 0");
    let err = analyzer.analyze_query(&query).unwrap_err();
    assert!(matches!(err, AnalyzerError::AggregateNotAllowed { .. }));
}

#[test]
fn analyze_aggregate_in_select_allowed() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("SELECT COUNT(id) FROM users");
    let result = analyzer.analyze_query(&query).unwrap();
    assert_eq!(result.output_schema[0].1, DataType::Int64);
}

#[test]
fn analyze_aggregate_in_having_allowed() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("SELECT age FROM users GROUP BY age HAVING COUNT(id) > 1");
    let result = analyzer.analyze_query(&query).unwrap();
    assert!(expect_select(&result).having.is_some());
}

// ── Window function validation ──────────────────────────────

#[test]
fn analyze_window_function_without_over_rejected() {
    let err = analyze_expr_with_users("ROW_NUMBER()").unwrap_err();
    assert!(matches!(err, AnalyzerError::WindowNotAllowed { .. }));
}

#[test]
fn analyze_window_function_in_where_rejected() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("SELECT id FROM users WHERE SUM(age) OVER () > 0");
    let err = analyzer.analyze_query(&query).unwrap_err();
    assert!(matches!(err, AnalyzerError::WindowNotAllowed { .. }));
}

// ── COALESCE / NULLIF / GREATEST / LEAST IR variants ────────

#[test]
fn analyze_coalesce_produces_coalesce_node() {
    let expr = analyze_expr_with_users("COALESCE(name, email, 'unknown')").unwrap();
    assert_eq!(expr.data_type, DataType::Text);
    match &expr.kind {
        TypedExprKind::Coalesce(args) => {
            assert_eq!(args.len(), 3);
        }
        other => panic!("expected Coalesce, got {:?}", std::mem::discriminant(other)),
    }
}

#[test]
fn analyze_coalesce_unifies_types_with_implicit_casts() {
    // COALESCE(int32, int64) → unified to Int64 with implicit cast on first arg
    let expr = analyze_expr_with_users("COALESCE(age, 9999999999)").unwrap();
    assert_eq!(expr.data_type, DataType::Int64);
    match &expr.kind {
        TypedExprKind::Coalesce(args) => {
            // First arg (Int32) should be wrapped in implicit Cast → Int64
            assert!(matches!(
                &args[0].kind,
                TypedExprKind::Cast {
                    cast_context: crate::sql::types::CastContext::Implicit,
                    ..
                }
            ));
            assert_eq!(args[0].data_type, DataType::Int64);
            // Second arg (Int64) should NOT be cast
            assert_eq!(args[1].data_type, DataType::Int64);
            assert!(matches!(&args[1].kind, TypedExprKind::Constant(_)));
        }
        other => panic!("expected Coalesce, got {:?}", std::mem::discriminant(other)),
    }
}

#[test]
fn analyze_nullif_produces_nullif_node() {
    let expr = analyze_expr_with_users("NULLIF(name, 'deleted')").unwrap();
    assert_eq!(expr.data_type, DataType::Text);
    assert!(matches!(expr.kind, TypedExprKind::NullIf(_, _)));
}

#[test]
fn analyze_greatest_produces_minmax_node() {
    let expr = analyze_expr_with_users("GREATEST(age, 18)").unwrap();
    assert_eq!(expr.data_type, DataType::Int32);
    match &expr.kind {
        TypedExprKind::MinMax { args, is_greatest } => {
            assert_eq!(args.len(), 2);
            assert!(*is_greatest);
        }
        other => panic!("expected MinMax, got {:?}", std::mem::discriminant(other)),
    }
}

#[test]
fn analyze_least_produces_minmax_node() {
    let expr = analyze_expr_with_users("LEAST(age, 100)").unwrap();
    assert_eq!(expr.data_type, DataType::Int32);
    match &expr.kind {
        TypedExprKind::MinMax { is_greatest, .. } => {
            assert!(!*is_greatest);
        }
        other => panic!("expected MinMax, got {:?}", std::mem::discriminant(other)),
    }
}

// ── Query body structure ────────────────────────────────────

#[test]
fn analyze_select_produces_select_body() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("SELECT id FROM users");
    let result = analyzer.analyze_query(&query).unwrap();
    assert!(matches!(result.body, AnalyzedQueryBody::Select(_)));
}

#[test]
fn analyze_distinct_on() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("SELECT DISTINCT ON (age) id, name FROM users");
    let result = analyzer.analyze_query(&query).unwrap();
    let select = expect_select(&result);
    assert!(matches!(select.distinct, AnalyzedDistinct::DistinctOn(_)));
    if let AnalyzedDistinct::DistinctOn(ref exprs) = select.distinct {
        assert_eq!(exprs.len(), 1);
    }
}

#[test]
fn analyze_cte_query() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query(
        "WITH active_users AS (SELECT id, name FROM users WHERE active) \
         SELECT id FROM active_users",
    );
    let result = analyzer.analyze_query(&query).unwrap();
    assert_eq!(result.ctes.len(), 1);
    assert_eq!(result.ctes[0].name, "active_users");
    assert_eq!(result.ctes[0].columns.len(), 2);
    assert!(result.ctes[0].materialized.is_none());
}

// ── NULL-aware type unification (#715) ──────────────────────

#[test]
fn analyze_coalesce_null_adopts_non_null_type() {
    // COALESCE(age, NULL) → Int32 (NULL adopts age's type, not Text)
    let expr = analyze_expr_with_users("COALESCE(age, NULL)").unwrap();
    assert_eq!(expr.data_type, DataType::Int32);
    match &expr.kind {
        TypedExprKind::Coalesce(args) => {
            assert_eq!(args[0].data_type, DataType::Int32);
            // NULL should be retyped to Int32, not Text
            assert_eq!(args[1].data_type, DataType::Int32);
            assert!(args[1].is_null_constant());
        }
        other => panic!("expected Coalesce, got {:?}", std::mem::discriminant(other)),
    }
}

#[test]
fn analyze_case_null_else_adopts_result_type() {
    // CASE WHEN active THEN age ELSE NULL END → Int32
    let expr = analyze_expr_with_users("CASE WHEN active THEN age ELSE NULL END").unwrap();
    assert_eq!(expr.data_type, DataType::Int32);
}

#[test]
fn analyze_array_with_null_infers_element_type() {
    // ARRAY[1, NULL] → Int32[] (NULL adopts Int32)
    let expr = analyze_expr_with_users("ARRAY[1, NULL]").unwrap();
    assert_eq!(expr.data_type, DataType::Array(Box::new(DataType::Int32)));
}

#[test]
fn analyze_in_list_with_null() {
    // age IN (30, NULL) → Boolean (NULL adopts Int32)
    let expr = analyze_expr_with_users("age IN (30, NULL)").unwrap();
    assert_eq!(expr.data_type, DataType::Boolean);
}

// ── IN subquery validation ──────────────────────────────────

#[test]
fn analyze_in_subquery_multi_column_rejected() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query =
        parse_query("SELECT id FROM users WHERE id IN (SELECT order_id, amount FROM orders)");
    let err = analyzer.analyze_query(&query).unwrap_err();
    assert!(matches!(
        err,
        AnalyzerError::ScalarSubqueryMultipleColumns { got: 2 }
    ));
}

#[test]
fn analyze_in_subquery_single_column_ok() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("SELECT id FROM users WHERE id IN (SELECT order_id FROM orders)");
    assert!(analyzer.analyze_query(&query).is_ok());
}

// ── Set operation validation (#718) ─────────────────────────

#[test]
fn analyze_union_column_count_mismatch() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("SELECT id FROM users UNION SELECT order_id, amount FROM orders");
    let err = analyzer.analyze_query(&query).unwrap_err();
    assert!(matches!(
        err,
        AnalyzerError::SetOperationColumnMismatch { left: 1, right: 2 }
    ));
}

#[test]
fn analyze_union_unifies_types() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    // id (Int32) UNION ALL score (Float64) → unified to Float64
    let query = parse_query("SELECT id FROM users UNION ALL SELECT score FROM users");
    let result = analyzer.analyze_query(&query).unwrap();
    assert_eq!(result.output_schema[0].1, DataType::Float64);
}
