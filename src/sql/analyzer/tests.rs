//! Analyzer unit tests.
//!
//! Uses `MockCatalog` and `sqlparser` to test expression and query analysis.

use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

use crate::model::DataType;
use crate::sql::analyzer::catalog::MockCatalog;
use crate::sql::analyzer::scope::Scope;
use crate::sql::analyzer::types::*;
use crate::sql::analyzer::{Analyzer, AnalyzerError};

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

fn text_literal_value(expr: &TypedExpr) -> Option<&str> {
    match &expr.kind {
        TypedExprKind::Constant(crate::model::Value::Text(s)) => Some(s.as_str()),
        TypedExprKind::Cast { expr, .. } => text_literal_value(expr),
        _ => None,
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
            ("id".to_string(), DataType::Int32, false, None),
            ("name".to_string(), DataType::Text, true, None),
            ("age".to_string(), DataType::Int32, true, None),
            ("email".to_string(), DataType::Text, true, None),
            ("active".to_string(), DataType::Boolean, true, None),
            ("score".to_string(), DataType::Float64, true, None),
            ("created_at".to_string(), DataType::Timestamp, true, None),
        ],
    );
    Analyzer::analyze_expr_with_scope(&catalog, scope, &parse_expr(sql))
}

/// Like analyze_expr_with_users, but with aggregates disallowed (simulates WHERE context).
// Test infrastructure -- will be wired up when analyzer tests expand.
#[allow(dead_code)] // test: analyzer test helper
fn analyze_expr_no_aggregates(sql: &str) -> Result<TypedExpr, AnalyzerError> {
    let catalog = test_catalog();
    let mut scope = Scope::new();
    scope.add_table(
        "users",
        &[
            ("id".to_string(), DataType::Int32, false, None),
            ("name".to_string(), DataType::Text, true, None),
            ("age".to_string(), DataType::Int32, true, None),
            ("email".to_string(), DataType::Text, true, None),
            ("active".to_string(), DataType::Boolean, true, None),
            ("score".to_string(), DataType::Float64, true, None),
            ("created_at".to_string(), DataType::Timestamp, true, None),
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
        TypedExprKind::Constant(crate::model::Value::Int32(42))
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

#[test]
fn analyze_json_access_chain_reassociates_to_json_access_nodes() {
    let expr = analyze_expr_with_users("'{\"a\": {\"b\": 2}}'::jsonb -> 'a' -> 'b'").unwrap();
    assert_eq!(expr.data_type, DataType::Jsonb);

    let (outer_expr, outer_path, outer_op) = match &expr.kind {
        TypedExprKind::JsonAccess {
            expr,
            path,
            operator,
        } => (expr, path, operator),
        other => panic!("expected outer JsonAccess, got {:?}", other),
    };
    assert_eq!(*outer_op, JsonAccessOp::Arrow);
    assert_eq!(text_literal_value(outer_path), Some("b"));

    let (inner_path, inner_op) = match &outer_expr.kind {
        TypedExprKind::JsonAccess { path, operator, .. } => (path, operator),
        other => panic!("expected inner JsonAccess, got {:?}", other),
    };
    assert_eq!(*inner_op, JsonAccessOp::Arrow);
    assert_eq!(text_literal_value(inner_path), Some("a"));
}

#[test]
fn analyze_json_access_comparison_precedence_stays_binary_comparison() {
    let expr =
        analyze_expr_with_users("'{\"level\":\"senior\"}'::jsonb ->> 'level' = 'senior'").unwrap();
    assert_eq!(expr.data_type, DataType::Boolean);

    let (left, op, right) = match &expr.kind {
        TypedExprKind::BinaryOp { left, op, right } => (left, op, right),
        other => panic!("expected BinaryOp, got {:?}", other),
    };
    assert_eq!(*op, BinaryOp::Eq);

    match &left.kind {
        TypedExprKind::JsonAccess { path, operator, .. } => {
            assert_eq!(*operator, JsonAccessOp::LongArrow);
            assert_eq!(text_literal_value(path), Some("level"));
        }
        other => panic!("expected JsonAccess on comparison left, got {:?}", other),
    }

    assert!(matches!(
        right.kind,
        TypedExprKind::Constant(crate::model::Value::Text(ref s)) if s == "senior"
    ));
}

#[test]
fn analyze_json_access_is_null_precedence_stays_is_test_over_json_access() {
    let expr = analyze_expr_with_users(
        "'{\"settings\":{}}'::jsonb -> 'settings' ->> 'inputUiInfo' IS NULL",
    )
    .unwrap();
    assert_eq!(expr.data_type, DataType::Boolean);

    let inner = match &expr.kind {
        TypedExprKind::IsTest {
            expr,
            test,
            negated,
        } => {
            assert_eq!(*test, IsTestKind::Null);
            assert!(!negated);
            expr
        }
        other => panic!("expected IsTest, got {:?}", other),
    };

    match &inner.kind {
        TypedExprKind::JsonAccess { path, operator, .. } => {
            assert_eq!(*operator, JsonAccessOp::LongArrow);
            assert_eq!(text_literal_value(path), Some("inputUiInfo"));
        }
        other => panic!("expected JsonAccess under IS NULL, got {:?}", other),
    }
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

// ── ScalarArrayCmp (<> ANY) ──────────────────────────────────

#[test]
fn analyze_ne_any_array_uses_scalar_array_cmp() {
    // `age <> ANY(ARRAY[1, 2, 3])` should produce ScalarArrayCmp, not InList
    let expr = analyze_expr_with_users("age <> ANY(ARRAY[1, 2, 3])").unwrap();
    assert_eq!(expr.data_type, DataType::Boolean);
    match &expr.kind {
        TypedExprKind::ScalarArrayCmp {
            elems, op, use_or, ..
        } => {
            assert_eq!(elems.len(), 3);
            assert_eq!(*op, BinaryOp::NotEq);
            assert!(*use_or); // ANY = OR semantics
        }
        _ => panic!("expected ScalarArrayCmp, got {:?}", expr.kind),
    }
}

#[test]
fn analyze_ne_any_empty_array_uses_scalar_array_cmp() {
    // `age <> ANY(ARRAY[]::int[])` should produce ScalarArrayCmp with empty elems
    let expr = analyze_expr_with_users("age <> ANY(ARRAY[]::int[])").unwrap();
    assert_eq!(expr.data_type, DataType::Boolean);
    match &expr.kind {
        TypedExprKind::ScalarArrayCmp {
            elems, op, use_or, ..
        } => {
            assert!(elems.is_empty());
            assert_eq!(*op, BinaryOp::NotEq);
            assert!(*use_or);
        }
        _ => panic!("expected ScalarArrayCmp, got {:?}", expr.kind),
    }
}

#[test]
fn analyze_any_empty_array_coerces_untyped_text_literal() {
    let typed = analyze_expr_with_users("'1' = ANY(ARRAY[]::int[])").unwrap();
    assert_eq!(typed.data_type, DataType::Boolean);
    match &typed.kind {
        TypedExprKind::InList {
            expr,
            list,
            negated,
        } => {
            assert!(list.is_empty());
            assert!(!negated);
            assert_eq!(expr.data_type, DataType::Int32);
            assert!(matches!(expr.kind, TypedExprKind::Cast { .. }));
        }
        _ => panic!("expected InList, got {:?}", typed.kind),
    }
}

#[test]
fn analyze_any_empty_array_rejects_explicit_text_cast() {
    let err = analyze_expr_with_users("'1'::text = ANY(ARRAY[]::int[])").unwrap_err();
    assert!(matches!(
        err,
        AnalyzerError::OperatorTypeMismatch { ref operator, ref left, ref right }
            if operator == "=" && left == "text" && right == "integer"
    ));
}

#[test]
fn analyze_any_empty_array_rejects_explicit_name_cast() {
    let err = analyze_expr_with_users("'1'::name = ANY(ARRAY[]::int[])").unwrap_err();
    assert!(matches!(
        err,
        AnalyzerError::OperatorTypeMismatch { ref operator, ref left, ref right }
            if operator == "=" && left == "name" && right == "integer"
    ));
}

#[test]
fn analyze_any_empty_array_rejects_text_column() {
    let err = analyze_expr_with_users("name = ANY(ARRAY[]::int[])").unwrap_err();
    assert!(matches!(
        err,
        AnalyzerError::OperatorTypeMismatch { ref operator, ref left, ref right }
            if operator == "=" && left == "text" && right == "integer"
    ));
}

#[test]
fn analyze_any_non_empty_array_coerces_untyped_text_literal() {
    let typed = analyze_expr_with_users("'1' = ANY(ARRAY[1, 2])").unwrap();
    assert_eq!(typed.data_type, DataType::Boolean);
    match &typed.kind {
        TypedExprKind::InList {
            expr,
            list,
            negated,
        } => {
            assert_eq!(list.len(), 2);
            assert!(!negated);
            assert_eq!(expr.data_type, DataType::Int32);
            assert!(matches!(expr.kind, TypedExprKind::Cast { .. }));
            assert!(list.iter().all(|e| e.data_type == DataType::Int32));
        }
        _ => panic!("expected InList, got {:?}", typed.kind),
    }
}

#[test]
fn analyze_any_non_empty_array_accepts_untyped_null_literal() {
    let typed = analyze_expr_with_users("NULL = ANY(ARRAY[1, 2])").unwrap();
    assert_eq!(typed.data_type, DataType::Boolean);
    match &typed.kind {
        TypedExprKind::InList { expr, list, .. } => {
            assert_eq!(expr.data_type, DataType::Int32);
            assert!(expr.is_null_constant());
            assert_eq!(list.len(), 2);
            assert!(list.iter().all(|e| e.data_type == DataType::Int32));
        }
        _ => panic!("expected InList, got {:?}", typed.kind),
    }
}

#[test]
fn analyze_any_non_empty_array_mixed_unknown_rhs_literals_use_typed_comparison() {
    let typed = analyze_expr_with_users("1 = ANY(ARRAY[1, '2'])").unwrap();
    assert_eq!(typed.data_type, DataType::Boolean);
    match &typed.kind {
        TypedExprKind::InList {
            expr,
            list,
            negated,
        } => {
            assert!(!negated);
            assert_eq!(expr.data_type, DataType::Int32);
            assert_eq!(list.len(), 2);
            assert!(list.iter().all(|e| e.data_type == DataType::Int32));
        }
        _ => panic!("expected InList, got {:?}", typed.kind),
    }
}

#[test]
fn analyze_any_non_empty_array_mixed_explicit_text_like_rhs_literals_rejected() {
    // PG parity: explicit text-like RHS members are concrete. Mixing them with
    // concrete non-text members must fail ARRAY type resolution.
    let err = analyze_expr_with_users("'1' = ANY(ARRAY[1, '2'::varchar])").unwrap_err();
    assert!(matches!(
        err,
        AnalyzerError::TypesCannotBeMatched { ref context, .. } if context == "ARRAY"
    ));
}

#[test]
fn analyze_array_literal_rejects_incompatible_non_text_recovery_types() {
    // PG parity: UNKNOWN text literal must not force text[] when concrete
    // non-text members are themselves incompatible.
    let err = analyze_expr_with_users("ARRAY['x', 1, now()]").unwrap_err();
    assert!(matches!(
        err,
        AnalyzerError::TypesCannotBeMatched { ref context, .. } if context == "ARRAY"
    ));
}

#[test]
fn analyze_ne_any_non_empty_array_accepts_untyped_null_literal() {
    let typed = analyze_expr_with_users("NULL <> ANY(ARRAY[1, 2])").unwrap();
    assert_eq!(typed.data_type, DataType::Boolean);
    match &typed.kind {
        TypedExprKind::ScalarArrayCmp {
            expr,
            elems,
            op,
            use_or,
        } => {
            assert_eq!(expr.data_type, DataType::Int32);
            assert!(expr.is_null_constant());
            assert_eq!(elems.len(), 2);
            assert!(elems.iter().all(|e| e.data_type == DataType::Int32));
            assert_eq!(*op, BinaryOp::NotEq);
            assert!(*use_or);
        }
        _ => panic!("expected ScalarArrayCmp, got {:?}", typed.kind),
    }
}

#[test]
fn analyze_ne_any_non_empty_array_mixed_unknown_rhs_literals_use_typed_comparison() {
    let typed = analyze_expr_with_users("1 <> ANY(ARRAY[1, '2'])").unwrap();
    assert_eq!(typed.data_type, DataType::Boolean);
    match &typed.kind {
        TypedExprKind::ScalarArrayCmp {
            expr,
            elems,
            op,
            use_or,
        } => {
            assert_eq!(expr.data_type, DataType::Int32);
            assert_eq!(elems.len(), 2);
            assert!(elems.iter().all(|e| e.data_type == DataType::Int32));
            assert_eq!(*op, BinaryOp::NotEq);
            assert!(*use_or);
        }
        _ => panic!("expected ScalarArrayCmp, got {:?}", typed.kind),
    }
}

#[test]
fn analyze_any_non_empty_array_all_unknown_text_literals_reject_non_text_lhs() {
    let err = analyze_expr_with_users("1 = ANY(ARRAY['1', '2'])").unwrap_err();
    assert!(matches!(
        err,
        AnalyzerError::OperatorTypeMismatch { ref operator, ref left, ref right }
            if operator == "=" && left == "integer" && right == "text"
    ));
}

#[test]
fn analyze_any_non_empty_array_accepts_text_name_comparison() {
    let typed = analyze_expr_with_users("'x'::text = ANY(ARRAY['x'::name])").unwrap();
    assert_eq!(typed.data_type, DataType::Boolean);
    match &typed.kind {
        TypedExprKind::InList { expr, list, .. } => {
            assert_eq!(expr.data_type, DataType::Text);
            assert_eq!(list.len(), 1);
            assert!(list.iter().all(|e| e.data_type == DataType::Text));
        }
        _ => panic!("expected InList, got {:?}", typed.kind),
    }
}

#[test]
fn analyze_any_non_empty_array_rejects_explicit_text_cast() {
    let err = analyze_expr_with_users("'1'::text = ANY(ARRAY[1, 2])").unwrap_err();
    assert!(matches!(
        err,
        AnalyzerError::OperatorTypeMismatch { ref operator, ref left, ref right }
            if operator == "=" && left == "text" && right == "integer"
    ));
}

#[test]
fn analyze_any_non_empty_array_rejects_explicit_name_cast() {
    let err = analyze_expr_with_users("'1'::name = ANY(ARRAY[1, 2])").unwrap_err();
    assert!(matches!(
        err,
        AnalyzerError::OperatorTypeMismatch { ref operator, ref left, ref right }
            if operator == "=" && left == "name" && right == "integer"
    ));
}

#[test]
fn analyze_any_non_empty_array_rejects_text_column() {
    let err = analyze_expr_with_users("name = ANY(ARRAY[1, 2])").unwrap_err();
    assert!(matches!(
        err,
        AnalyzerError::OperatorTypeMismatch { ref operator, ref left, ref right }
            if operator == "=" && left == "text" && right == "integer"
    ));
}

#[test]
fn analyze_any_non_empty_array_respects_explicit_text_array_cast() {
    // PG parity: explicit RHS text-like array cast must be honored; no
    // literal-member recovery to integer is allowed.
    let err = analyze_expr_with_users("1 = ANY((ARRAY[1, '2'])::varchar[])").unwrap_err();
    assert!(matches!(
        err,
        AnalyzerError::OperatorTypeMismatch { ref operator, ref left, ref right }
            if operator == "=" && left == "integer" && right == "text"
    ));
}

#[test]
fn analyze_ne_any_non_empty_array_respects_explicit_text_array_cast() {
    let err = analyze_expr_with_users("1 <> ANY((ARRAY[1, '2'])::varchar[])").unwrap_err();
    assert!(matches!(
        err,
        AnalyzerError::OperatorTypeMismatch { ref operator, ref left, ref right }
            if operator == "<>" && left == "integer" && right == "text"
    ));
}

#[test]
fn analyze_ne_any_non_empty_array_rejects_explicit_text_cast() {
    let err = analyze_expr_with_users("'1'::text <> ANY(ARRAY[1, 2])").unwrap_err();
    assert!(matches!(
        err,
        AnalyzerError::OperatorTypeMismatch { ref operator, ref left, ref right }
            if operator == "<>" && left == "text" && right == "integer"
    ));
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

#[test]
fn analyze_simple_case_coerces_when_values_to_operand_type() {
    // Simple CASE: operand (Boolean) compared against WHEN values.
    //
    // PostgreSQL treats string literals as UNKNOWN and coerces them to the
    // operand's type for the internal `=` comparisons.
    let expr = analyze_expr_with_users("CASE active WHEN 't' THEN 1 ELSE 0 END").unwrap();
    assert_eq!(expr.data_type, DataType::Int32);
    match &expr.kind {
        TypedExprKind::Case {
            operand: Some(operand),
            when_clauses,
            ..
        } => {
            assert_eq!(operand.data_type, DataType::Boolean);
            assert_eq!(when_clauses.len(), 1);
            let (when_expr, _then_expr) = &when_clauses[0];
            assert_eq!(when_expr.data_type, DataType::Boolean);
            assert!(matches!(
                when_expr.kind,
                TypedExprKind::Cast {
                    target_type: DataType::Boolean,
                    cast_context: crate::sql::types::CastContext::Implicit,
                    ..
                }
            ));
        }
        other => panic!("expected Case, got {:?}", std::mem::discriminant(other)),
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

#[test]
fn analyze_advisory_lock_two_arg_accepts_integer_integer() {
    let expr = analyze_expr_with_users("pg_advisory_lock(1, 2)").unwrap();
    assert_eq!(expr.data_type, DataType::Text);
    match &expr.kind {
        TypedExprKind::FunctionCall { func, args, .. } => {
            assert_eq!(func.name, "PG_ADVISORY_LOCK");
            assert_eq!(args.len(), 2);
            assert_eq!(args[0].data_type, DataType::Int32);
            assert_eq!(args[1].data_type, DataType::Int32);
        }
        _ => panic!("expected FunctionCall"),
    }
}

#[test]
fn analyze_advisory_lock_two_arg_rejects_bigint_bigint() {
    let err = analyze_expr_with_users("pg_advisory_lock(1::bigint, 2::bigint)").unwrap_err();
    assert!(matches!(
        err,
        AnalyzerError::FunctionNotFound {
            ref name,
            arg_types
        } if name == "PG_ADVISORY_LOCK" && arg_types == vec![DataType::Int64, DataType::Int64]
    ));
}

#[test]
fn analyze_generate_subscripts_coerces_dim_and_reverse() {
    let expr = analyze_expr_with_users("generate_subscripts(ARRAY[1,2], '1', 'true')").unwrap();
    assert_eq!(expr.data_type, DataType::Int32);
    match &expr.kind {
        TypedExprKind::FunctionCall { func, args, .. } => {
            assert_eq!(func.name, "GENERATE_SUBSCRIPTS");
            assert_eq!(args.len(), 3);
            assert_eq!(args[1].data_type, DataType::Int32);
            assert_eq!(args[2].data_type, DataType::Boolean);
            assert!(matches!(
                args[1].kind,
                TypedExprKind::Cast {
                    target_type: DataType::Int32,
                    cast_context: crate::sql::types::CastContext::Implicit,
                    ..
                }
            ));
            assert!(matches!(
                args[2].kind,
                TypedExprKind::Cast {
                    target_type: DataType::Boolean,
                    cast_context: crate::sql::types::CastContext::Implicit,
                    ..
                }
            ));
        }
        _ => panic!("expected FunctionCall"),
    }
}

#[test]
fn analyze_generate_subscripts_rejects_non_array_first_arg() {
    let err = analyze_expr_with_users("generate_subscripts(1, 1)").unwrap_err();
    assert!(matches!(
        err,
        AnalyzerError::FunctionNotFound {
            ref name,
            arg_types
        } if name == "GENERATE_SUBSCRIPTS" && arg_types == vec![DataType::Int32, DataType::Int32]
    ));
}

#[test]
fn analyze_pg_get_indexdef_coerces_column_no_and_pretty() {
    let expr = analyze_expr_with_users("pg_get_indexdef(1, '1', 'true')").unwrap();
    assert_eq!(expr.data_type, DataType::Text);
    match &expr.kind {
        TypedExprKind::FunctionCall { func, args, .. } => {
            assert_eq!(func.name, "PG_GET_INDEXDEF");
            assert_eq!(args.len(), 3);
            assert_eq!(args[0].data_type, DataType::Int64);
            assert_eq!(args[1].data_type, DataType::Int32);
            assert_eq!(args[2].data_type, DataType::Boolean);
            assert!(matches!(
                args[1].kind,
                TypedExprKind::Cast {
                    target_type: DataType::Int32,
                    cast_context: crate::sql::types::CastContext::Implicit,
                    ..
                }
            ));
            assert!(matches!(
                args[2].kind,
                TypedExprKind::Cast {
                    target_type: DataType::Boolean,
                    cast_context: crate::sql::types::CastContext::Implicit,
                    ..
                }
            ));
        }
        _ => panic!("expected FunctionCall"),
    }
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
        TypedExprKind::Constant(crate::model::Value::Date(_))
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
    assert_eq!(
        result.output_schema[0],
        ("id".to_string(), DataType::Int32, None)
    );
    assert_eq!(
        result.output_schema[1],
        ("name".to_string(), DataType::Text, None)
    );
    assert!(expect_select(&result).where_clause.is_some());
}

#[test]
fn analyze_select_version_infers_function_column_alias() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("SELECT version()");
    let result = analyzer.analyze_query(&query).unwrap();

    assert_eq!(result.output_schema.len(), 1);
    assert_eq!(result.output_schema[0].0, "version");
    assert_eq!(result.output_schema[0].1, DataType::Text);
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
fn analyze_group_by_rejects_ungrouped_select_column() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("SELECT age, name FROM users GROUP BY age");
    let err = analyzer.analyze_query(&query).unwrap_err();
    assert!(matches!(err, AnalyzerError::UngroupedColumn { ref name } if name == "users.name"));
}

#[test]
fn analyze_group_by_allows_expression_of_grouped_column() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("SELECT age + 1, COUNT(id) FROM users GROUP BY age");
    let result = analyzer.analyze_query(&query).unwrap();
    assert_eq!(expect_select(&result).group_by.len(), 1);
    assert_eq!(result.output_schema.len(), 2);
}

#[test]
fn analyze_aggregate_without_group_by_rejects_plain_column() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("SELECT age, COUNT(id) FROM users");
    let err = analyzer.analyze_query(&query).unwrap_err();
    assert!(matches!(err, AnalyzerError::UngroupedColumn { ref name } if name == "users.age"));
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

#[test]
fn analyze_values_query_body() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("VALUES (1, 'a'), (2, 'b')");
    let result = analyzer.analyze_query(&query).unwrap();

    assert_eq!(result.output_schema.len(), 2);
    assert_eq!(
        result.output_schema[0],
        ("column1".to_string(), DataType::Int32, None)
    );
    assert_eq!(
        result.output_schema[1],
        ("column2".to_string(), DataType::Text, None)
    );
    match &result.body {
        AnalyzedQueryBody::Values(rows) => {
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].len(), 2);
            assert_eq!(rows[1].len(), 2);
        }
        _ => panic!("expected Values body"),
    }
}

#[test]
fn analyze_values_query_order_by_position_resolves_to_column() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("VALUES (1), (2) ORDER BY 1");
    let result = analyzer.analyze_query(&query).unwrap();

    assert_eq!(result.order_by.len(), 1);
    match &result.order_by[0].expr.kind {
        TypedExprKind::ColumnRef { column_index, .. } => assert_eq!(*column_index, 0),
        other => panic!(
            "expected ColumnRef, got {:?}",
            std::mem::discriminant(other)
        ),
    }
}

#[test]
fn analyze_values_query_unifies_column_types_with_implicit_cast() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("VALUES (1), (9999999999)");
    let result = analyzer.analyze_query(&query).unwrap();

    assert_eq!(result.output_schema.len(), 1);
    assert_eq!(result.output_schema[0].1, DataType::Int64);
    let AnalyzedQueryBody::Values(rows) = &result.body else {
        panic!("expected Values");
    };
    assert!(matches!(
        rows[0][0].kind,
        TypedExprKind::Cast {
            cast_context: crate::sql::types::CastContext::Implicit,
            ..
        }
    ));
    assert_eq!(rows[0][0].data_type, DataType::Int64);
    assert_eq!(rows[1][0].data_type, DataType::Int64);
}

#[test]
fn analyze_values_rejects_mismatched_row_width() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("VALUES (1), (1, 2)");
    let err = analyzer.analyze_query(&query).unwrap_err();
    assert!(matches!(err, AnalyzerError::Unsupported(_)));
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
fn analyze_comparison_prefers_non_text_target_on_right_literal() {
    // age (Int32) > '9' (Text) -> right side should be implicitly cast to Int32
    let expr = analyze_expr_with_users("age > '9'").unwrap();
    assert_eq!(expr.data_type, DataType::Boolean);
    match &expr.kind {
        TypedExprKind::BinaryOp { left, right, .. } => {
            assert_eq!(left.data_type, DataType::Int32);
            assert_eq!(right.data_type, DataType::Int32);
            assert!(matches!(
                &right.kind,
                TypedExprKind::Cast {
                    target_type: DataType::Int32,
                    cast_context: crate::sql::types::CastContext::Implicit,
                    ..
                }
            ));
        }
        _ => panic!("expected BinaryOp"),
    }
}

#[test]
fn analyze_comparison_prefers_non_text_target_on_left_literal() {
    // '9' (Text) < age (Int32) -> left side should be implicitly cast to Int32
    let expr = analyze_expr_with_users("'9' < age").unwrap();
    assert_eq!(expr.data_type, DataType::Boolean);
    match &expr.kind {
        TypedExprKind::BinaryOp { left, right, .. } => {
            assert_eq!(left.data_type, DataType::Int32);
            assert_eq!(right.data_type, DataType::Int32);
            assert!(matches!(
                &left.kind,
                TypedExprKind::Cast {
                    target_type: DataType::Int32,
                    cast_context: crate::sql::types::CastContext::Implicit,
                    ..
                }
            ));
        }
        _ => panic!("expected BinaryOp"),
    }
}

#[test]
fn analyze_arithmetic_coerces_text_literal_to_numeric() {
    // PostgreSQL UNKNOWN literal behavior: '100' is coerced to the numeric operator context.
    let expr = analyze_expr_with_users("'100' + 50").unwrap();
    assert_eq!(expr.data_type, DataType::Int32);
    match &expr.kind {
        TypedExprKind::BinaryOp { left, op, right } => {
            assert_eq!(*op, BinaryOp::Add);
            assert_eq!(left.data_type, DataType::Int32);
            assert_eq!(right.data_type, DataType::Int32);
            assert!(matches!(
                &left.kind,
                TypedExprKind::Cast {
                    target_type: DataType::Int32,
                    cast_context: crate::sql::types::CastContext::Implicit,
                    ..
                }
            ));
        }
        _ => panic!("expected BinaryOp"),
    }
}

#[test]
fn analyze_arithmetic_coerces_text_literal_on_right_side() {
    let expr = analyze_expr_with_users("50 + '100'").unwrap();
    assert_eq!(expr.data_type, DataType::Int32);
    match &expr.kind {
        TypedExprKind::BinaryOp { left, op, right } => {
            assert_eq!(*op, BinaryOp::Add);
            assert_eq!(left.data_type, DataType::Int32);
            assert_eq!(right.data_type, DataType::Int32);
            assert!(matches!(
                &right.kind,
                TypedExprKind::Cast {
                    target_type: DataType::Int32,
                    cast_context: crate::sql::types::CastContext::Implicit,
                    ..
                }
            ));
        }
        _ => panic!("expected BinaryOp"),
    }
}

#[test]
fn analyze_arithmetic_rejects_explicit_text_literal_plus_int() {
    // Explicit typing prevents UNKNOWN-literal coercion.
    let err = analyze_expr_with_users("'100'::text + 50").unwrap_err();
    assert!(matches!(
        err,
        AnalyzerError::OperatorTypeMismatch { ref operator, ref left, ref right }
            if operator == "+" && left == "text" && right == "integer"
    ));
}

#[test]
fn analyze_arithmetic_rejects_text_column_plus_int() {
    let err = analyze_expr_with_users("name + 50").unwrap_err();
    assert!(matches!(
        err,
        AnalyzerError::OperatorTypeMismatch { ref operator, ref left, ref right }
            if operator == "+" && left == "text" && right == "integer"
    ));
}

#[test]
fn analyze_arithmetic_rejects_text_column_plus_text_column() {
    let err = analyze_expr_with_users("name + name").unwrap_err();
    assert!(matches!(
        err,
        AnalyzerError::OperatorTypeMismatch { ref operator, ref left, ref right }
            if operator == "+" && left == "text" && right == "text"
    ));

    let sql: crate::sql::error::SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42883");
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

#[test]
fn analyze_tuple_in_subquery_builds_tuple_variant() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query(
        "SELECT id FROM users WHERE (id, name) IN (SELECT user_id, status FROM orders)",
    );
    let analyzed = analyzer.analyze_query(&query).unwrap();
    let select = expect_select(&analyzed);
    let where_expr = select.where_clause.as_ref().expect("missing WHERE clause");
    match &where_expr.kind {
        TypedExprKind::TupleInSubquery {
            exprs,
            subquery,
            negated,
        } => {
            assert_eq!(exprs.len(), 2);
            assert_eq!(subquery.output_schema.len(), 2);
            assert!(!negated);
        }
        other => panic!(
            "expected TupleInSubquery, got {:?}",
            std::mem::discriminant(other)
        ),
    }
}

#[test]
fn analyze_is_distinct_from_builds_typed_variant() {
    let expr = analyze_expr_with_users("id IS DISTINCT FROM age").unwrap();
    match &expr.kind {
        TypedExprKind::IsDistinctFrom { negated, .. } => assert!(!negated),
        other => panic!(
            "expected IsDistinctFrom, got {:?}",
            std::mem::discriminant(other)
        ),
    }

    let expr = analyze_expr_with_users("id IS NOT DISTINCT FROM age").unwrap();
    match &expr.kind {
        TypedExprKind::IsDistinctFrom { negated, .. } => assert!(*negated),
        other => panic!(
            "expected IsDistinctFrom, got {:?}",
            std::mem::discriminant(other)
        ),
    }
}

#[test]
fn analyze_any_subquery_produces_anyall_typed_expr() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query =
        parse_query("SELECT id FROM users WHERE id = ANY (ARRAY(SELECT user_id FROM orders))");
    let analyzed = analyzer.analyze_query(&query).unwrap();
    let select = expect_select(&analyzed);
    let where_expr = select.where_clause.as_ref().expect("missing WHERE clause");
    match &where_expr.kind {
        TypedExprKind::AnyAll {
            op,
            is_all,
            subquery,
            ..
        } => {
            assert_eq!(*op, BinaryOp::Eq);
            assert!(!*is_all);
            assert_eq!(subquery.output_schema.len(), 1);
        }
        other => panic!("expected AnyAll, got {:?}", std::mem::discriminant(other)),
    }
}

#[test]
fn analyze_all_subquery_produces_anyall_typed_expr() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query =
        parse_query("SELECT id FROM users WHERE id >= ALL (ARRAY(SELECT user_id FROM orders))");
    let analyzed = analyzer.analyze_query(&query).unwrap();
    let select = expect_select(&analyzed);
    let where_expr = select.where_clause.as_ref().expect("missing WHERE clause");
    match &where_expr.kind {
        TypedExprKind::AnyAll {
            op,
            is_all,
            subquery,
            ..
        } => {
            assert_eq!(*op, BinaryOp::GtEq);
            assert!(*is_all);
            assert_eq!(subquery.output_schema.len(), 1);
        }
        other => panic!("expected AnyAll, got {:?}", std::mem::discriminant(other)),
    }
}

#[test]
fn analyze_any_subquery_multi_column_rejected() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query(
        "SELECT id FROM users WHERE id = ANY (ARRAY(SELECT order_id, amount FROM orders))",
    );
    let err = analyzer.analyze_query(&query).unwrap_err();
    assert!(matches!(
        err,
        AnalyzerError::ScalarSubqueryMultipleColumns { got: 2 }
    ));
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

    let AnalyzedQueryBody::SetOperation { left, right, .. } = &result.body else {
        panic!("expected SetOperation body");
    };
    assert_eq!(left.output_schema[0].1, DataType::Float64);
    assert_eq!(right.output_schema[0].1, DataType::Float64);

    // Left arm should be wrapped with a coercing projection (CAST) since it
    // originally produced Int32.
    let AnalyzedQueryBody::Select(left_select) = &left.body else {
        panic!("expected wrapped SELECT for left arm");
    };
    assert_eq!(left_select.projection.len(), 1);
    assert!(matches!(
        left_select.projection[0].expr.kind,
        TypedExprKind::Cast { .. }
    ));
    assert!(matches!(
        left_select.from[0].kind,
        AnalyzedTableRefKind::Subquery(_)
    ));
}

#[test]
fn analyze_union_incompatible_types_rejected() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("SELECT true UNION SELECT 1");
    let err = analyzer.analyze_query(&query).unwrap_err();
    assert!(matches!(err, AnalyzerError::TypesCannotBeMatched { .. }));
}

// ── DML statement analysis ──────────────────────────────────

fn parse_statement(sql: &str) -> sqlparser::ast::Statement {
    let dialect = PostgreSqlDialect {};
    Parser::parse_sql(&dialect, sql)
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
}

#[test]
fn analyze_insert_values() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let stmt = parse_statement("INSERT INTO users (id, name) VALUES (1, 'alice')");
    let result = analyzer.analyze_statement(&stmt).unwrap();
    match result {
        AnalyzedStatement::Insert(ins) => {
            assert_eq!(ins.table_name, "public.users");
            assert_eq!(ins.target_columns, vec![0, 1]); // id=0, name=1
            match &ins.source {
                AnalyzedInsertSource::Values(rows) => {
                    assert_eq!(rows.len(), 1);
                    assert_eq!(rows[0].len(), 2);
                }
                _ => panic!("expected Values source"),
            }
            assert!(ins.on_conflict.is_none());
            assert!(ins.returning.is_none());
        }
        _ => panic!("expected AnalyzedStatement::Insert"),
    }
}

#[test]
fn analyze_insert_values_preserves_quoted_target_columns() {
    let catalog = MockCatalog::builder()
        .table(
            "t_case_cols",
            vec![
                ("x", DataType::Int32, false),
                ("y", DataType::Int32, true),
                ("Y", DataType::Int32, true),
            ],
        )
        .build();
    let mut analyzer = Analyzer::new(&catalog);
    let stmt = parse_statement("INSERT INTO t_case_cols (x, y, \"Y\") VALUES (1, 10, 20)");
    let result = analyzer.analyze_statement(&stmt).unwrap();

    match result {
        AnalyzedStatement::Insert(ins) => {
            assert_eq!(ins.target_columns, vec![0, 1, 2]);
        }
        _ => panic!("expected AnalyzedStatement::Insert"),
    }
}

#[test]
fn analyze_insert_default_values() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let stmt = parse_statement("INSERT INTO users DEFAULT VALUES");
    let result = analyzer.analyze_statement(&stmt).unwrap();
    match result {
        AnalyzedStatement::Insert(ins) => {
            assert!(matches!(ins.source, AnalyzedInsertSource::DefaultValues));
        }
        _ => panic!("expected AnalyzedStatement::Insert"),
    }
}

#[test]
fn analyze_insert_column_not_found() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let stmt = parse_statement("INSERT INTO users (id, nonexistent) VALUES (1, 'x')");
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    assert!(matches!(err, AnalyzerError::DmlColumnNotFound { .. }));
}

#[test]
fn analyze_insert_column_count_mismatch() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let stmt = parse_statement("INSERT INTO users (id, name) VALUES (1, 'a', 'extra')");
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    assert!(matches!(
        err,
        AnalyzerError::InsertColumnCountMismatch {
            columns: 2,
            values: 3
        }
    ));
}

#[test]
fn analyze_delete_simple() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let stmt = parse_statement("DELETE FROM users WHERE id = 1");
    let result = analyzer.analyze_statement(&stmt).unwrap();
    match result {
        AnalyzedStatement::Delete(del) => {
            assert_eq!(del.table_name, "public.users");
            assert!(del.where_clause.is_some());
            assert!(del.using.is_empty());
            assert!(del.returning.is_none());
        }
        _ => panic!("expected AnalyzedStatement::Delete"),
    }
}

#[test]
fn analyze_delete_no_where() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let stmt = parse_statement("DELETE FROM users");
    let result = analyzer.analyze_statement(&stmt).unwrap();
    match result {
        AnalyzedStatement::Delete(del) => {
            assert!(del.where_clause.is_none());
        }
        _ => panic!("expected AnalyzedStatement::Delete"),
    }
}

#[test]
fn analyze_delete_where_not_boolean() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let stmt = parse_statement("DELETE FROM users WHERE name");
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    assert!(matches!(err, AnalyzerError::DmlWhereNotBoolean { .. }));
}

#[test]
fn analyze_update_simple() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let stmt = parse_statement("UPDATE users SET name = 'bob' WHERE id = 1");
    let result = analyzer.analyze_statement(&stmt).unwrap();
    match result {
        AnalyzedStatement::Update(upd) => {
            assert_eq!(upd.table_name, "public.users");
            assert_eq!(upd.assignments.len(), 1);
            assert_eq!(upd.assignments[0].0, 1); // name is column index 1
            assert!(upd.where_clause.is_some());
            assert!(upd.from.is_empty());
            assert!(upd.returning.is_none());
        }
        _ => panic!("expected AnalyzedStatement::Update"),
    }
}

#[test]
fn analyze_update_column_not_found() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let stmt = parse_statement("UPDATE users SET nonexistent = 'x' WHERE id = 1");
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    assert!(matches!(err, AnalyzerError::DmlColumnNotFound { .. }));
}

#[test]
fn analyze_update_where_not_boolean() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let stmt = parse_statement("UPDATE users SET name = 'bob' WHERE name");
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    assert!(matches!(err, AnalyzerError::DmlWhereNotBoolean { .. }));
}

#[test]
fn analyze_update_multiple_assignments() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let stmt = parse_statement("UPDATE users SET name = 'bob', age = 30 WHERE id = 1");
    let result = analyzer.analyze_statement(&stmt).unwrap();
    match result {
        AnalyzedStatement::Update(upd) => {
            assert_eq!(upd.assignments.len(), 2);
            // name=col 1, age=col 2
            let col_indices: Vec<usize> = upd.assignments.iter().map(|(i, _)| *i).collect();
            assert!(col_indices.contains(&1)); // name
            assert!(col_indices.contains(&2)); // age
        }
        _ => panic!("expected AnalyzedStatement::Update"),
    }
}

#[test]
fn analyze_update_set_default_assignment() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let stmt = parse_statement("UPDATE users SET age = DEFAULT WHERE id = 1");
    let result = analyzer.analyze_statement(&stmt).unwrap();
    match result {
        AnalyzedStatement::Update(upd) => {
            assert_eq!(upd.assignments.len(), 1);
            assert_eq!(upd.assignments[0].0, 2); // age
            assert!(matches!(upd.assignments[0].1.kind, TypedExprKind::Default));
            assert_eq!(upd.assignments[0].1.data_type, DataType::Int32);
        }
        _ => panic!("expected AnalyzedStatement::Update"),
    }
}

#[test]
fn analyze_insert_on_conflict_do_nothing() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let stmt =
        parse_statement("INSERT INTO users (id, name) VALUES (1, 'alice') ON CONFLICT DO NOTHING");
    let result = analyzer.analyze_statement(&stmt).unwrap();
    match result {
        AnalyzedStatement::Insert(ins) => {
            assert!(matches!(
                ins.on_conflict,
                Some(AnalyzedOnConflict::DoNothing)
            ));
        }
        _ => panic!("expected AnalyzedStatement::Insert"),
    }
}

#[test]
fn analyze_table_not_found_in_dml() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let stmt = parse_statement("DELETE FROM nonexistent WHERE id = 1");
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    assert!(matches!(err, AnalyzerError::TableNotFound(_)));
}

// ── ON CONFLICT DO UPDATE analysis (test 119 scenarios) ────

#[test]
fn analyze_insert_on_conflict_do_update() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let stmt = parse_statement(
        "INSERT INTO users (id, name) VALUES (1, 'alice') \
         ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name",
    );
    let result = analyzer.analyze_statement(&stmt).unwrap();
    match result {
        AnalyzedStatement::Insert(ins) => {
            match &ins.on_conflict {
                Some(AnalyzedOnConflict::DoUpdate { assignments, .. }) => {
                    assert_eq!(assignments.len(), 1, "expected 1 assignment");
                    // column index 1 = "name" (0=id, 1=name)
                    assert_eq!(assignments[0].0, 1);
                }
                other => panic!("expected DoUpdate, got {:?}", other),
            }
        }
        _ => panic!("expected AnalyzedStatement::Insert"),
    }
}

#[test]
fn analyze_insert_on_conflict_on_constraint_target() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let stmt = parse_statement(
        "INSERT INTO users (id, name) VALUES (1, 'alice') \
         ON CONFLICT ON CONSTRAINT users_pkey DO UPDATE SET name = EXCLUDED.name",
    );
    let result = analyzer.analyze_statement(&stmt).unwrap();
    match result {
        AnalyzedStatement::Insert(ins) => match &ins.on_conflict {
            Some(AnalyzedOnConflict::DoUpdate { target, .. }) => {
                assert!(matches!(
                    target,
                    Some(AnalyzedConflictTarget::Constraint(name)) if name == "users_pkey"
                ));
            }
            other => panic!("expected DoUpdate, got {:?}", other),
        },
        _ => panic!("expected AnalyzedStatement::Insert"),
    }
}

#[test]
fn analyze_insert_on_conflict_do_update_excluded_plus_expr() {
    // Mirrors test 119 Case 1: SET parent_id = EXCLUDED.parent_id + 1
    let catalog = MockCatalog::builder()
        .table(
            "t_child",
            vec![
                ("id", DataType::Int32, false),
                ("parent_id", DataType::Int32, true),
            ],
        )
        .build();
    let mut analyzer = Analyzer::new(&catalog);
    let stmt = parse_statement(
        "INSERT INTO t_child(id, parent_id) VALUES (1, 1) \
         ON CONFLICT (id) DO UPDATE SET parent_id = EXCLUDED.parent_id + 1",
    );
    let result = analyzer.analyze_statement(&stmt).unwrap();
    match result {
        AnalyzedStatement::Insert(ins) => {
            // MockCatalog resolves unqualified names as "public.<name>"
            assert!(ins.table_name.ends_with("t_child"));
            match &ins.on_conflict {
                Some(AnalyzedOnConflict::DoUpdate { assignments, .. }) => {
                    assert_eq!(assignments.len(), 1);
                    // column index 1 = "parent_id" (0=id, 1=parent_id)
                    assert_eq!(assignments[0].0, 1);
                    // The RHS should be a BinaryOp (EXCLUDED.parent_id + 1)
                    assert!(
                        matches!(assignments[0].1.kind, TypedExprKind::BinaryOp { .. }),
                        "expected BinaryOp, got {:?}",
                        assignments[0].1.kind
                    );
                }
                other => panic!("expected DoUpdate, got {:?}", other),
            }
        }
        _ => panic!("expected AnalyzedStatement::Insert"),
    }
}

#[test]
fn analyze_insert_on_conflict_do_update_set_default() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let stmt = parse_statement(
        "INSERT INTO users (id, name, age) VALUES (1, 'alice', 10) \
         ON CONFLICT (id) DO UPDATE SET age = DEFAULT",
    );
    let result = analyzer.analyze_statement(&stmt).unwrap();
    match result {
        AnalyzedStatement::Insert(ins) => match &ins.on_conflict {
            Some(AnalyzedOnConflict::DoUpdate { assignments, .. }) => {
                assert_eq!(assignments.len(), 1);
                assert_eq!(assignments[0].0, 2); // age
                assert!(matches!(assignments[0].1.kind, TypedExprKind::Default));
                assert_eq!(assignments[0].1.data_type, DataType::Int32);
            }
            other => panic!("expected DoUpdate, got {:?}", other),
        },
        _ => panic!("expected AnalyzedStatement::Insert"),
    }
}

#[test]
fn analyze_insert_on_conflict_do_update_target_table_qualified_column() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let stmt = parse_statement(
        "INSERT INTO users (id, name, age) VALUES (1, 'alice', 10) \
         ON CONFLICT (id) DO UPDATE SET age = users.age + 1",
    );
    let result = analyzer.analyze_statement(&stmt).unwrap();
    match result {
        AnalyzedStatement::Insert(ins) => match &ins.on_conflict {
            Some(AnalyzedOnConflict::DoUpdate { assignments, .. }) => {
                assert_eq!(assignments.len(), 1);
                assert_eq!(assignments[0].0, 2); // age
                assert!(
                    matches!(assignments[0].1.kind, TypedExprKind::BinaryOp { .. }),
                    "expected BinaryOp, got {:?}",
                    assignments[0].1.kind
                );
            }
            other => panic!("expected DoUpdate, got {:?}", other),
        },
        _ => panic!("expected AnalyzedStatement::Insert"),
    }
}

#[test]
fn analyze_insert_returning_schema_qualified_target_column() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let stmt = parse_statement(
        "INSERT INTO users (id, name, age) VALUES (1, 'alice', 10) \
         RETURNING public.users.id",
    );
    let result = analyzer.analyze_statement(&stmt).unwrap();
    match result {
        AnalyzedStatement::Insert(ins) => {
            let returning = ins.returning.expect("expected RETURNING projection");
            assert_eq!(returning.len(), 1);
            assert_eq!(returning[0].output_name, "id");
            assert_eq!(returning[0].expr.data_type, DataType::Int32);
            assert!(
                matches!(returning[0].expr.kind, TypedExprKind::ColumnRef { .. }),
                "expected ColumnRef, got {:?}",
                returning[0].expr.kind
            );
        }
        _ => panic!("expected AnalyzedStatement::Insert"),
    }
}

// ── Type coercion at analysis time (test 155 scenarios) ────

#[test]
fn analyze_insert_jsonb_into_int_column_errors() {
    // Mirrors test 155: INSERT INTO t_int(id, i) VALUES (1, '{"a":1}'::jsonb)
    // The Analyzer correctly rejects JSONB→INT at analysis time — this is the
    // "cannot cast type JSONB to INT" error that test 155 expects.
    let catalog = MockCatalog::builder()
        .table(
            "t_int",
            vec![("id", DataType::Int32, false), ("i", DataType::Int32, true)],
        )
        .build();
    let mut analyzer = Analyzer::new(&catalog);
    let stmt = parse_statement("INSERT INTO t_int(id, i) VALUES (1, '{\"a\":1}'::jsonb)");
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    assert!(
        matches!(err, AnalyzerError::AssignmentTypeMismatch { .. }),
        "expected AssignmentTypeMismatch, got {:?}",
        err
    );
}

#[test]
fn analyze_update_jsonb_into_int_column_errors() {
    // Mirrors test 155: UPDATE t_int SET i = ('{"a":1}'::jsonb) WHERE id = 1
    // The Analyzer correctly rejects JSONB→INT at analysis time.
    let catalog = MockCatalog::builder()
        .table(
            "t_int",
            vec![("id", DataType::Int32, false), ("i", DataType::Int32, true)],
        )
        .build();
    let mut analyzer = Analyzer::new(&catalog);
    let stmt = parse_statement("UPDATE t_int SET i = ('{\"a\":1}'::jsonb) WHERE id = 1");
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    assert!(
        matches!(err, AnalyzerError::AssignmentTypeMismatch { .. }),
        "expected AssignmentTypeMismatch, got {:?}",
        err
    );
}

// ── ConflictBehavior derivation (Smell 1 verification) ─────

#[test]
fn conflict_behavior_from_analyzed_on_conflict() {
    use crate::sql::dml::{ConflictBehavior, ConflictTarget};

    // None → Error
    let none: Option<AnalyzedOnConflict> = None;
    let behavior = match &none {
        Some(AnalyzedOnConflict::DoNothing) => ConflictBehavior::DoNothing,
        Some(AnalyzedOnConflict::DoUpdate { .. }) => ConflictBehavior::DoUpdate { target: None },
        None => ConflictBehavior::Error,
    };
    assert_eq!(behavior, ConflictBehavior::Error);

    // DoNothing → DoNothing
    let do_nothing = Some(AnalyzedOnConflict::DoNothing);
    let behavior = match &do_nothing {
        Some(AnalyzedOnConflict::DoNothing) => ConflictBehavior::DoNothing,
        Some(AnalyzedOnConflict::DoUpdate { .. }) => ConflictBehavior::DoUpdate { target: None },
        None => ConflictBehavior::Error,
    };
    assert_eq!(behavior, ConflictBehavior::DoNothing);

    let do_update = Some(AnalyzedOnConflict::DoUpdate {
        target: Some(AnalyzedConflictTarget::Constraint("users_pkey".to_string())),
        assignments: vec![],
        where_clause: None,
    });
    let behavior = match &do_update {
        Some(AnalyzedOnConflict::DoNothing) => ConflictBehavior::DoNothing,
        Some(AnalyzedOnConflict::DoUpdate { target, .. }) => {
            let target = match target {
                Some(AnalyzedConflictTarget::Columns(cols)) => {
                    Some(ConflictTarget::Columns(cols.clone()))
                }
                Some(AnalyzedConflictTarget::Constraint(name)) => {
                    Some(ConflictTarget::Constraint(name.clone()))
                }
                None => None,
            };
            ConflictBehavior::DoUpdate { target }
        }
        None => ConflictBehavior::Error,
    };
    assert_eq!(
        behavior,
        ConflictBehavior::DoUpdate {
            target: Some(ConflictTarget::Constraint("users_pkey".to_string()))
        }
    );
}

// ── Parameter analysis tests ─────────────────────────────────

#[test]
fn analyze_select_with_parameter() {
    // WHERE id = $1 → param typed as Int32 from column
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT * FROM users WHERE id = $1");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let result = analyzer.analyze_statement(&stmt).unwrap();
    let types = analyzer.finalize_param_types().unwrap();
    assert_eq!(types, vec![DataType::Int32]);
    assert!(matches!(result, AnalyzedStatement::Query(_)));
}

#[test]
fn analyze_unknown_plus_unknown_reports_ambiguous_operator() {
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT $1 + $1");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: crate::sql::error::SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42725");
    assert!(sql.to_string().contains("operator is not unique"));
}

// ── Mixed-unknown operator ambiguity tests (PG 42725 parity) ────

#[test]
fn analyze_param_plus_literal_reports_ambiguous_operator() {
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT $1 + '1'");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: crate::sql::error::SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42725");
}

#[test]
fn analyze_param_minus_literal_reports_ambiguous_operator() {
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT $1 - '1'");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: crate::sql::error::SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42725");
}

#[test]
fn analyze_param_mul_literal_reports_ambiguous_operator() {
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT $1 * '1'");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: crate::sql::error::SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42725");
}

#[test]
fn analyze_param_div_literal_reports_ambiguous_operator() {
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT $1 / '1'");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: crate::sql::error::SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42725");
}

#[test]
fn analyze_param_mod_literal_reports_ambiguous_operator() {
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT $1 % '1'");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: crate::sql::error::SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42725");
}

#[test]
fn analyze_param_bitand_literal_reports_ambiguous_operator() {
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT $1 & '1'");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: crate::sql::error::SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42725");
}

#[test]
fn analyze_param_bitor_literal_reports_ambiguous_operator() {
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT $1 | '1'");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: crate::sql::error::SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42725");
}

#[test]
fn analyze_param_shl_literal_reports_ambiguous_operator() {
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT $1 << '1'");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: crate::sql::error::SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42725");
}

#[test]
fn analyze_param_shr_literal_reports_ambiguous_operator() {
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT $1 >> '1'");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: crate::sql::error::SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42725");
}

#[test]
fn analyze_null_plus_literal_reports_ambiguous_operator() {
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT NULL + '1'");
    let mut analyzer = Analyzer::new_with_params(&catalog, 0, &[]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: crate::sql::error::SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42725");
}

#[test]
fn analyze_null_plus_null_reports_ambiguous_operator() {
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT NULL + NULL");
    let mut analyzer = Analyzer::new_with_params(&catalog, 0, &[]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: crate::sql::error::SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42725");
}

#[test]
fn analyze_param_plus_null_reports_ambiguous_operator() {
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT $1 + NULL");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: crate::sql::error::SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42725");
}

#[test]
fn analyze_null_plus_param_reports_ambiguous_operator() {
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT NULL + $1");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: crate::sql::error::SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42725");
}

#[test]
fn analyze_param_concat_literal_resolves_to_text() {
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT $1 || '1'");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    analyzer.analyze_statement(&stmt).unwrap();
    let types = analyzer.finalize_param_types().unwrap();
    assert_eq!(types, vec![DataType::Text]);
}

#[test]
fn analyze_literal_plus_literal_reports_ambiguous_operator() {
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT '1' + '1'");
    let mut analyzer = Analyzer::new_with_params(&catalog, 0, &[]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: crate::sql::error::SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42725");
}

#[test]
fn analyze_explicit_cast_plus_literal_stays_42883() {
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT $1 + '1'::text");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: crate::sql::error::SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42883");
}

#[test]
fn analyze_param_bitxor_literal_reports_ambiguous_operator() {
    // sqlparser 0.40 doesn't parse `#` as BitwiseXor in PG dialect,
    // so we construct the AST directly to exercise the ambiguity path.
    use sqlparser::ast::{self as ast, BinaryOperator};
    let catalog = test_catalog();
    let expr = ast::Expr::BinaryOp {
        left: Box::new(ast::Expr::Value(ast::Value::Placeholder("$1".into()))),
        op: BinaryOperator::BitwiseXor,
        right: Box::new(ast::Expr::Value(ast::Value::SingleQuotedString("1".into()))),
    };
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let err = analyzer.analyze_expr(&expr).unwrap_err();
    let sql: crate::sql::error::SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42725");
}

#[test]
fn analyze_unknown_plus_int_infers_integer_and_succeeds() {
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT $1 + 1");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    analyzer.analyze_statement(&stmt).unwrap();
    let types = analyzer.finalize_param_types().unwrap();
    assert_eq!(types, vec![DataType::Int32]);
}

#[test]
fn analyze_unknown_concat_unknown_resolves_to_text() {
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT $1 || $2");
    let mut analyzer = Analyzer::new_with_params(&catalog, 2, &[None, None]);
    analyzer.analyze_statement(&stmt).unwrap();
    let types = analyzer.finalize_param_types().unwrap();
    assert_eq!(types, vec![DataType::Text, DataType::Text]);
}

// ── Extended 42725 coverage (#911) ──

#[test]
fn analyze_unknown_minus_unknown_reports_42725() {
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT $1 - $2");
    let mut analyzer = Analyzer::new_with_params(&catalog, 2, &[None, None]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: crate::sql::error::SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42725");
}

#[test]
fn analyze_unknown_mul_unknown_reports_42725() {
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT $1 * $2");
    let mut analyzer = Analyzer::new_with_params(&catalog, 2, &[None, None]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: crate::sql::error::SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42725");
}

#[test]
fn analyze_unknown_div_unknown_reports_42725() {
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT $1 / $2");
    let mut analyzer = Analyzer::new_with_params(&catalog, 2, &[None, None]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: crate::sql::error::SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42725");
}

#[test]
fn analyze_unknown_mod_unknown_reports_42725() {
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT $1 % $2");
    let mut analyzer = Analyzer::new_with_params(&catalog, 2, &[None, None]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: crate::sql::error::SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42725");
}

#[test]
fn analyze_unknown_exp_unknown_reports_42725() {
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT $1 ^ $2");
    let mut analyzer = Analyzer::new_with_params(&catalog, 2, &[None, None]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: crate::sql::error::SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42725");
}

#[test]
fn analyze_unknown_bitand_unknown_reports_42725() {
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT $1 & $2");
    let mut analyzer = Analyzer::new_with_params(&catalog, 2, &[None, None]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: crate::sql::error::SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42725");
}

#[test]
fn analyze_unknown_bitor_unknown_reports_42725() {
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT $1 | $2");
    let mut analyzer = Analyzer::new_with_params(&catalog, 2, &[None, None]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: crate::sql::error::SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42725");
}

#[test]
fn analyze_unknown_shl_unknown_reports_42725() {
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT $1 << $2");
    let mut analyzer = Analyzer::new_with_params(&catalog, 2, &[None, None]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: crate::sql::error::SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42725");
}

#[test]
fn analyze_unknown_shr_unknown_reports_42725() {
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT $1 >> $2");
    let mut analyzer = Analyzer::new_with_params(&catalog, 2, &[None, None]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: crate::sql::error::SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42725");
}

#[test]
fn analyze_param_exp_literal_reports_42725() {
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT $1 ^ '1'");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: crate::sql::error::SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42725");
}

#[test]
fn analyze_literal_concat_literal_resolves_to_text() {
    let catalog = test_catalog();
    let query = parse_query("SELECT 'hello' || 'world'");
    let mut analyzer = Analyzer::new_with_params(&catalog, 0, &[]);
    let result = analyzer.analyze_query(&query).unwrap();
    assert_eq!(result.output_schema.len(), 1);
    assert_eq!(result.output_schema[0].1, DataType::Text);
}

#[test]
fn analyze_int_plus_int_succeeds() {
    let catalog = test_catalog();
    let query = parse_query("SELECT 1 + 2");
    let mut analyzer = Analyzer::new_with_params(&catalog, 0, &[]);
    let result = analyzer.analyze_query(&query).unwrap();
    assert_eq!(result.output_schema.len(), 1);
    assert_eq!(result.output_schema[0].1, DataType::Int32);
}

#[test]
fn analyze_explicit_cast_params_succeed() {
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT $1::int + $2::int");
    let mut analyzer = Analyzer::new_with_params(&catalog, 2, &[None, None]);
    analyzer.analyze_statement(&stmt).unwrap();
    let types = analyzer.finalize_param_types().unwrap();
    assert_eq!(types, vec![DataType::Int32, DataType::Int32]);
}

#[test]
fn analyze_text_column_plus_text_column_stays_42883() {
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT name + name FROM users");
    let mut analyzer = Analyzer::new_with_params(&catalog, 0, &[]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: crate::sql::error::SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42883");
}

#[test]
fn analyze_insert_with_parameters() {
    // INSERT INTO users (id, name) VALUES ($1, $2) → params typed from columns
    let catalog = test_catalog();
    let stmt = parse_statement("INSERT INTO users (id, name) VALUES ($1, $2)");
    let mut analyzer = Analyzer::new_with_params(&catalog, 2, &[None, None]);
    analyzer.analyze_statement(&stmt).unwrap();
    let types = analyzer.finalize_param_types().unwrap();
    assert_eq!(types, vec![DataType::Int32, DataType::Text]);
}

#[test]
fn analyze_parameter_limit() {
    // LIMIT $1 → param typed as Int64
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT * FROM users LIMIT $1");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    analyzer.analyze_statement(&stmt).unwrap();
    let types = analyzer.finalize_param_types().unwrap();
    assert_eq!(types, vec![DataType::Int64]);
}

#[test]
fn analyze_limit_rejects_row_variable() {
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT * FROM users LIMIT id");
    let mut analyzer = Analyzer::new_with_params(&catalog, 0, &[]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    assert!(
        matches!(err, AnalyzerError::Unsupported(msg) if msg.contains("argument of LIMIT must not contain variables"))
    );
}

#[test]
fn analyze_offset_rejects_row_variable() {
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT * FROM users OFFSET id");
    let mut analyzer = Analyzer::new_with_params(&catalog, 0, &[]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    assert!(
        matches!(err, AnalyzerError::Unsupported(msg) if msg.contains("argument of OFFSET must not contain variables"))
    );
}

#[test]
fn analyze_parameter_limit_in_set_operation() {
    // UNION ... LIMIT $1 → param typed as Int64 on set-op query body path
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT id FROM users UNION ALL SELECT id FROM users LIMIT $1");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    analyzer.analyze_statement(&stmt).unwrap();
    let types = analyzer.finalize_param_types().unwrap();
    assert_eq!(types, vec![DataType::Int64]);
}

#[test]
fn analyze_parameter_limit_offset_in_values_query() {
    // VALUES ... LIMIT/OFFSET params typed as Int64 on VALUES query body path
    let catalog = test_catalog();
    let stmt = parse_statement("VALUES (1), (2) LIMIT $1 OFFSET $2");
    let mut analyzer = Analyzer::new_with_params(&catalog, 2, &[None, None]);
    analyzer.analyze_statement(&stmt).unwrap();
    let types = analyzer.finalize_param_types().unwrap();
    assert_eq!(types, vec![DataType::Int64, DataType::Int64]);
}

#[test]
fn analyze_parameter_like_typed_as_text() {
    // WHERE name LIKE $1 → param typed as Text from LIKE context
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT * FROM users WHERE name LIKE $1");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    analyzer.analyze_statement(&stmt).unwrap();
    let types = analyzer.finalize_param_types().unwrap();
    assert_eq!(types, vec![DataType::Text]);
}

#[test]
fn analyze_parameter_ilike_typed_as_text() {
    // WHERE name ILIKE $1 → param typed as Text from ILIKE context
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT * FROM users WHERE name ILIKE $1");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    analyzer.analyze_statement(&stmt).unwrap();
    let types = analyzer.finalize_param_types().unwrap();
    assert_eq!(types, vec![DataType::Text]);
}

#[test]
fn analyze_unresolved_parameter_in_projection_defaults_to_text() {
    // SELECT target-list context resolves unknown parameter to Text.
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT $1");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    analyzer.analyze_statement(&stmt).unwrap();
    let types = analyzer.finalize_param_types().unwrap();
    assert_eq!(types, vec![DataType::Text]);
}

#[test]
fn analyze_pg_typeof_unknown_param_returns_42p18_on_finalize() {
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT pg_typeof($1)");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    analyzer.analyze_statement(&stmt).unwrap();
    let err = analyzer.finalize_param_types().unwrap_err();
    let sql: crate::sql::error::SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42P18");
}

#[test]
fn analyze_parameter_with_client_oid() {
    // Client provides INT4 OID → respected even without contextual typing
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT $1");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[Some(DataType::Int32)]);
    analyzer.analyze_statement(&stmt).unwrap();
    let types = analyzer.finalize_param_types().unwrap();
    assert_eq!(types, vec![DataType::Int32]);
}

#[test]
fn analyze_parameter_in_vector_function() {
    // cosine_distance(vector, $1) resolves $1 to vector from function context.
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT cosine_distance('[1,2,3]'::vector, $1)");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    analyzer.analyze_statement(&stmt).unwrap();
    let types = analyzer.finalize_param_types().unwrap();
    assert_eq!(types, vec![DataType::Vector(0)]);
}

#[test]
fn analyze_parameter_explicit_cast() {
    // $1::int4 → param typed as Int4
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT $1::int4");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let result = analyzer.analyze_statement(&stmt).unwrap();
    let types = analyzer.finalize_param_types().unwrap();
    assert_eq!(types, vec![DataType::Int32]);
    // The result should be a Parameter node (not Cast), since we resolve
    // the param type directly from the explicit cast target.
    if let AnalyzedStatement::Query(q) = result {
        if let AnalyzedQueryBody::Select(s) = &q.body {
            assert!(matches!(
                s.projection[0].expr.kind,
                TypedExprKind::Parameter { index: 0 }
            ));
        }
    }
}

#[test]
fn analyze_parameter_explicit_cast_binary_op_is_not_ambiguous() {
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT $1::int + $2::int");
    let mut analyzer = Analyzer::new_with_params(&catalog, 2, &[None, None]);
    analyzer.analyze_statement(&stmt).unwrap();
    let types = analyzer.finalize_param_types().unwrap();
    assert_eq!(types, vec![DataType::Int32, DataType::Int32]);
}

#[test]
fn analyze_parameter_in_list() {
    // WHERE id IN ($1, $2) → params typed from column
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT * FROM users WHERE id IN ($1, $2)");
    let mut analyzer = Analyzer::new_with_params(&catalog, 2, &[None, None]);
    analyzer.analyze_statement(&stmt).unwrap();
    let types = analyzer.finalize_param_types().unwrap();
    assert_eq!(types, vec![DataType::Int32, DataType::Int32]);
}

#[test]
fn analyze_parameter_coalesce() {
    // COALESCE($1, 42) → param typed as Int32
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT COALESCE($1, 42)");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    analyzer.analyze_statement(&stmt).unwrap();
    let types = analyzer.finalize_param_types().unwrap();
    assert_eq!(types, vec![DataType::Int32]);
}

#[test]
fn analyze_parameter_between() {
    // WHERE id BETWEEN $1 AND $2 → params typed from column
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT * FROM users WHERE id BETWEEN $1 AND $2");
    let mut analyzer = Analyzer::new_with_params(&catalog, 2, &[None, None]);
    analyzer.analyze_statement(&stmt).unwrap();
    let types = analyzer.finalize_param_types().unwrap();
    assert_eq!(types, vec![DataType::Int32, DataType::Int32]);
}

#[test]
fn analyze_parameter_zero_rejected() {
    // $0 is not a valid parameter index — test the helper directly
    let err = Analyzer::parse_placeholder_index("$0").unwrap_err();
    assert!(err.to_string().contains("$0"));
    // Ensure $1 still works
    assert_eq!(Analyzer::parse_placeholder_index("$1").unwrap(), 0);
}

#[test]
fn analyze_parameter_boolean_where() {
    // WHERE $1 → param typed as Boolean from ensure_boolean
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT * FROM users WHERE $1");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    analyzer.analyze_statement(&stmt).unwrap();
    let types = analyzer.finalize_param_types().unwrap();
    assert_eq!(types, vec![DataType::Boolean]);
}

#[test]
fn analyze_parameter_case_when_boolean() {
    // Regression: CASE WHEN $1 THEN ... → param typed as Boolean
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT CASE WHEN $1 THEN 1 ELSE 0 END FROM users");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    analyzer.analyze_statement(&stmt).unwrap();
    let types = analyzer.finalize_param_types().unwrap();
    assert_eq!(types, vec![DataType::Boolean]);
}

#[test]
fn analyze_parameter_inconsistent_types_detected() {
    // Regression: same $1 in incompatible contexts → InconsistentParameterTypes
    // id is Int32, active is Boolean — common_type(Int32, Boolean) = None
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT * FROM users WHERE id = $1 AND active = $1");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    assert!(
        err.to_string().contains("inconsistent types"),
        "expected InconsistentParameterTypes, got: {}",
        err,
    );
}

#[test]
fn analyze_parameter_zero_sqlstate() {
    // Regression: $0 should produce InvalidParameterUsage (42P02), not Unsupported
    let err = Analyzer::parse_placeholder_index("$0").unwrap_err();
    assert!(
        matches!(err, AnalyzerError::InvalidParameterUsage { index: 0, .. }),
        "expected InvalidParameterUsage, got: {:?}",
        err,
    );
}

#[test]
fn analyze_parameter_error_sqlstate_mapping() {
    // Regression: parameter errors map to correct SQLSTATEs via SqlError
    use crate::sql::error::SqlError;

    // IndeterminateParameterType → 42P18
    let ae = AnalyzerError::IndeterminateParameterType { index: 1 };
    let sql: SqlError = ae.into();
    assert_eq!(sql.sqlstate(), "42P18");

    // InconsistentParameterTypes → 42P18
    let ae = AnalyzerError::InconsistentParameterTypes {
        index: 1,
        first: DataType::Int32,
        second: DataType::Boolean,
    };
    let sql: SqlError = ae.into();
    assert_eq!(sql.sqlstate(), "42P18");

    // InvalidParameterUsage → 42P02
    let ae = AnalyzerError::InvalidParameterUsage {
        index: 0,
        context: "test".into(),
    };
    let sql: SqlError = ae.into();
    assert_eq!(sql.sqlstate(), "42P02");
}

#[test]
fn analyze_parameter_keeps_first_inferred_type_across_compatible_contexts() {
    // Regression: avoid widening inferred param type, which can desync
    // TypedExpr parameter node types from finalize_param_types().
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT * FROM users WHERE id = $1 LIMIT $1");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    analyzer.analyze_statement(&stmt).unwrap();
    let types = analyzer.finalize_param_types().unwrap();
    assert_eq!(types, vec![DataType::Int32]);
}

#[test]
fn analyze_parameter_with_client_oid_does_not_use_inference_conflict_path() {
    // Regression: client-typed params should bypass inference-conflict paths.
    // Keep predicates type-compatible (both Int32) so this test does not
    // depend on operator permissiveness outside parameter inference logic.
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT * FROM users WHERE id = $1 AND age = $1");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[Some(DataType::Int32)]);
    analyzer.analyze_statement(&stmt).unwrap();
    let types = analyzer.finalize_param_types().unwrap();
    assert_eq!(types, vec![DataType::Int32]);
}

// -- Collation propagation tests --

fn collation_catalog() -> MockCatalog {
    MockCatalog::builder()
        .collation("de", "de")
        .table_with_collations(
            "t1",
            vec![
                ("id", DataType::Int32, false, None),
                ("a", DataType::Text, true, Some("de")),
            ],
        )
        .table_with_collations(
            "t2",
            vec![
                ("id", DataType::Int32, false, None),
                ("a", DataType::Text, true, Some("de")),
            ],
        )
        .build()
}

/// Helper: extract collation name from a TypedExpr (if wrapped in Collate).
fn extract_collation_name(expr: &TypedExpr) -> Option<String> {
    crate::sql::expr::collation_aware::extract_collation(expr)
}

#[test]
fn collation_propagation_through_cte_select() {
    let catalog = collation_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("WITH cte AS (SELECT a FROM t1) SELECT a FROM cte ORDER BY a");
    let result = analyzer.analyze_query(&query).unwrap();

    let sel = expect_select(&result);
    // The projected column 'a' should carry the 'de' collation.
    assert_eq!(
        extract_collation_name(&sel.projection[0].expr),
        Some("de".to_string()),
        "CTE SELECT body must propagate collation"
    );
}

#[test]
fn collation_propagation_through_cte_union() {
    let catalog = collation_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query(
        "WITH cte AS (SELECT a FROM t1 UNION ALL SELECT a FROM t2) SELECT a FROM cte ORDER BY a",
    );
    let result = analyzer.analyze_query(&query).unwrap();

    let sel = expect_select(&result);
    // Collation must survive the UNION ALL boundary inside a CTE.
    assert_eq!(
        extract_collation_name(&sel.projection[0].expr),
        Some("de".to_string()),
        "CTE UNION body must propagate collation from left arm"
    );
}

#[test]
fn collation_propagation_through_coercion_wrapped_union() {
    // When set-op arms have different types (e.g. TEXT vs VARCHAR), the arm
    // is coercion-wrapped. Collation must survive the wrapping.
    let catalog = MockCatalog::builder()
        .collation("de", "de")
        .table_with_collations("t_text", vec![("a", DataType::Text, true, Some("de"))])
        .table_with_collations(
            "t_varchar",
            vec![("a", DataType::Varchar(255), true, Some("de"))],
        )
        .build();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query(
        "WITH cte AS (SELECT a FROM t_varchar UNION ALL SELECT a FROM t_text) \
         SELECT a FROM cte",
    );
    let result = analyzer.analyze_query(&query).unwrap();

    let sel = expect_select(&result);
    assert_eq!(
        extract_collation_name(&sel.projection[0].expr),
        Some("de".to_string()),
        "Coercion-wrapped UNION arm must preserve collation"
    );
}

#[test]
fn wildcard_carries_collation() {
    let catalog = collation_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("SELECT * FROM t1");
    let result = analyzer.analyze_query(&query).unwrap();

    let sel = expect_select(&result);
    // Column 'a' (index 1) should have collation 'de'.
    assert_eq!(
        extract_collation_name(&sel.projection[1].expr),
        Some("de".to_string()),
        "SELECT * must carry collation from scope column"
    );
    // Column 'id' (index 0) should have no collation.
    assert_eq!(
        extract_collation_name(&sel.projection[0].expr),
        None,
        "Non-collated column should have no collation"
    );
}

#[test]
fn qualified_wildcard_carries_collation() {
    let catalog = collation_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("SELECT t1.* FROM t1");
    let result = analyzer.analyze_query(&query).unwrap();

    let sel = expect_select(&result);
    assert_eq!(
        extract_collation_name(&sel.projection[1].expr),
        Some("de".to_string()),
        "SELECT t.* must carry collation from scope column"
    );
}

#[test]
fn derived_table_propagates_collation() {
    let catalog = collation_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("SELECT a FROM (SELECT a FROM t1) AS sub ORDER BY a");
    let result = analyzer.analyze_query(&query).unwrap();

    let sel = expect_select(&result);
    assert_eq!(
        extract_collation_name(&sel.projection[0].expr),
        Some("de".to_string()),
        "Derived table must propagate collation"
    );
}

#[test]
fn using_merged_column_carries_collation() {
    // Explicit SELECT a from a USING join must carry collation from the
    // merged column, consistent with the wildcard path.
    let catalog = collation_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("SELECT a FROM t1 JOIN t2 USING (a)");
    let result = analyzer.analyze_query(&query).unwrap();

    let sel = expect_select(&result);
    assert_eq!(
        extract_collation_name(&sel.projection[0].expr),
        Some("de".to_string()),
        "Explicit SELECT a with USING join must carry collation"
    );
}

// ── ANY($1) parameter inference (Prisma compat, #1059) ──────

#[test]
fn any_with_unresolved_text_parameter_infers_array_type() {
    // Prisma schema engine sends `WHERE nspname = ANY($1)` with OID=0.
    // The analyzer must infer $1 as Array(Text) from the left operand type.
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let query = parse_query("SELECT name FROM users WHERE name = ANY($1)");
    let _result = analyzer.analyze_query(&query).unwrap();

    let param_types = analyzer.finalize_param_types().unwrap();
    assert_eq!(param_types, vec![DataType::Array(Box::new(DataType::Text))]);
}

#[test]
fn any_with_unresolved_int_parameter_infers_int_array() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let query = parse_query("SELECT id FROM users WHERE id = ANY($1)");
    let _result = analyzer.analyze_query(&query).unwrap();

    let param_types = analyzer.finalize_param_types().unwrap();
    assert_eq!(
        param_types,
        vec![DataType::Array(Box::new(DataType::Int32))]
    );
}

#[test]
fn any_with_typed_array_parameter_still_works() {
    // Client provides OID for text[] — should still work.
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new_with_params(
        &catalog,
        1,
        &[Some(DataType::Array(Box::new(DataType::Text)))],
    );
    let query = parse_query("SELECT name FROM users WHERE name = ANY($1)");
    let _result = analyzer.analyze_query(&query).unwrap();

    let param_types = analyzer.finalize_param_types().unwrap();
    assert_eq!(param_types, vec![DataType::Array(Box::new(DataType::Text))]);
}

#[test]
fn any_with_cast_parameter_works() {
    // `$1::text[]` — explicit SQL cast should work (existing path).
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let query = parse_query("SELECT name FROM users WHERE name = ANY($1::text[])");
    let _result = analyzer.analyze_query(&query).unwrap();

    let param_types = analyzer.finalize_param_types().unwrap();
    assert_eq!(param_types, vec![DataType::Array(Box::new(DataType::Text))]);
}

#[test]
fn any_with_mixed_unknown_array_and_unresolved_parameter_infers_integer() {
    // PG parity regression: mixed UNKNOWN/non-UNKNOWN array members should not
    // lock `$1` to Text before scalar-array comparison typing.
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let query = parse_query("SELECT 1 = ANY(ARRAY[1, '2', $1])");
    let _result = analyzer.analyze_query(&query).unwrap();

    let param_types = analyzer.finalize_param_types().unwrap();
    assert_eq!(param_types, vec![DataType::Int32]);
}

#[test]
fn array_literal_with_mixed_unknown_and_unresolved_parameter_infers_integer() {
    // PG parity: plain ARRAY literal should infer $1 from concrete non-text
    // members instead of deferring to finalize(42P18).
    let catalog = test_catalog();
    let stmt = parse_statement("SELECT ARRAY[1, '2', $1]");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let result = analyzer.analyze_statement(&stmt).unwrap();
    let param_types = analyzer.finalize_param_types().unwrap();
    assert_eq!(param_types, vec![DataType::Int32]);
    let AnalyzedStatement::Query(query) = result else {
        panic!("expected query statement");
    };
    assert_eq!(
        query.output_schema[0].1,
        DataType::Array(Box::new(DataType::Int32))
    );
}

#[test]
fn any_with_mixed_unknown_array_and_explicit_text_parameter_is_rejected() {
    // PG parity: explicit text-like RHS members are concrete, so the ARRAY
    // itself is invalid before scalar-array comparison typing.
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let query = parse_query("SELECT 1 = ANY(ARRAY[1, '2', $1::text])");
    let err = analyzer.analyze_query(&query).unwrap_err();
    assert!(matches!(
        err,
        AnalyzerError::TypesCannotBeMatched { ref context, .. } if context == "ARRAY"
    ));
}

#[test]
fn any_with_unresolved_parameter_and_varchar_array_infers_text() {
    // PG parity: unknown-typed LHS against varchar(n)[] resolves to TEXT,
    // not varchar(n).
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let query = parse_query("SELECT $1 = ANY(ARRAY['a']::varchar(3)[])");
    let _result = analyzer.analyze_query(&query).unwrap();

    let param_types = analyzer.finalize_param_types().unwrap();
    assert_eq!(param_types, vec![DataType::Text]);
}

#[test]
fn any_subquery_with_unresolved_parameter_infers_scalar_type() {
    // Unknown-typed bind params on the LHS of ANY(subquery) should be inferred
    // from the subquery output type (PG parity).
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let query =
        parse_query("SELECT id FROM users WHERE $1 = ANY (ARRAY(SELECT user_id FROM orders))");
    let _result = analyzer.analyze_query(&query).unwrap();

    let param_types = analyzer.finalize_param_types().unwrap();
    assert_eq!(param_types, vec![DataType::Int32]);
}

#[test]
fn any_subquery_with_unresolved_parameter_and_text_rhs_infers_text() {
    // Regression: when both sides are nominally Text, unresolved parameters
    // must still be recorded as inferred to avoid 42P18 at finalize.
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let query = parse_query("SELECT id FROM users WHERE $1 = ANY (ARRAY(SELECT name FROM users))");
    let _result = analyzer.analyze_query(&query).unwrap();

    let param_types = analyzer.finalize_param_types().unwrap();
    assert_eq!(param_types, vec![DataType::Text]);
}

#[test]
fn any_subquery_with_unresolved_parameter_and_varchar_rhs_infers_text() {
    // PG parity: ANY/ALL text-like inference should not lock unresolved params
    // to varchar typmods.
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let query = parse_query("SELECT $1 = ANY (ARRAY(SELECT 'a'::varchar(3)))");
    let _result = analyzer.analyze_query(&query).unwrap();

    let param_types = analyzer.finalize_param_types().unwrap();
    assert_eq!(param_types, vec![DataType::Text]);
}

#[test]
fn any_subquery_with_unresolved_parameter_and_name_rhs_infers_name() {
    // Text-seeded unresolved LHS parameter should adopt RHS Name type in
    // ANY(subquery) context (PG parity).
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let query = parse_query("SELECT $1 = ANY (ARRAY(SELECT 'x'::name))");
    let _result = analyzer.analyze_query(&query).unwrap();

    let param_types = analyzer.finalize_param_types().unwrap();
    assert_eq!(param_types, vec![DataType::Name]);
}

#[test]
fn all_subquery_with_unresolved_parameter_and_text_rhs_infers_text() {
    // Same inference contract as ANY(subquery): unresolved LHS params must be
    // inferred from RHS type even when both seed as Text.
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let query = parse_query("SELECT id FROM users WHERE $1 = ALL (ARRAY(SELECT name FROM users))");
    let _result = analyzer.analyze_query(&query).unwrap();

    let param_types = analyzer.finalize_param_types().unwrap();
    assert_eq!(param_types, vec![DataType::Text]);
}

#[test]
fn any_subquery_accepts_text_name_comparison() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new(&catalog);
    let query = parse_query("SELECT id FROM users WHERE 'x'::text = ANY (ARRAY(SELECT 'x'::name))");
    let result = analyzer.analyze_query(&query).unwrap();
    let sel = expect_select(&result);
    let where_expr = sel.where_clause.as_ref().expect("missing WHERE");
    match &where_expr.kind {
        TypedExprKind::AnyAll {
            expr,
            op,
            subquery,
            is_all,
        } => {
            assert_eq!(expr.data_type, DataType::Text);
            assert_eq!(*op, BinaryOp::Eq);
            assert!(!*is_all);
            assert_eq!(subquery.output_schema[0].1, DataType::Name);
        }
        other => panic!("expected AnyAll, got {:?}", std::mem::discriminant(other)),
    }
}

#[test]
fn any_empty_array_with_unresolved_parameter_infers_element_type() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let query = parse_query("SELECT id FROM users WHERE $1 = ANY(ARRAY[]::int[])");
    let _result = analyzer.analyze_query(&query).unwrap();

    let param_types = analyzer.finalize_param_types().unwrap();
    assert_eq!(param_types, vec![DataType::Int32]);
}

#[test]
fn any_empty_name_array_with_unresolved_parameter_infers_name() {
    // Repro: `$1 = ANY(ARRAY[]::name[])` should infer `$1` as Name, not Text.
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let query = parse_query("SELECT $1 = ANY(ARRAY[]::name[])");
    let _result = analyzer.analyze_query(&query).unwrap();

    let param_types = analyzer.finalize_param_types().unwrap();
    assert_eq!(param_types, vec![DataType::Name]);
}

#[test]
fn any_subquery_with_text_typed_parameter_is_rejected() {
    let catalog = test_catalog();
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[Some(DataType::Text)]);
    let query =
        parse_query("SELECT id FROM users WHERE $1 = ANY (ARRAY(SELECT user_id FROM orders))");
    let err = analyzer.analyze_query(&query).unwrap_err();
    assert!(matches!(
        err,
        AnalyzerError::OperatorTypeMismatch { ref operator, ref left, ref right }
            if operator == "=" && left == "text" && right == "integer"
    ));
}
