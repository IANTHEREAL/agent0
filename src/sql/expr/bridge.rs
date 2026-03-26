//! Bridge: AST Expr -> Analyzer -> eval_typed_expr.
//!
//! These functions convert raw `sqlparser::ast::Expr` values into `TypedExpr`
//! via the Analyzer, then evaluate them with the typed evaluator. This replaces
//! the legacy `eval_expr` / `eval_join_expr` path for DML and utility code.

use crate::model::{DataType, Row, TableSchema, Value};
use crate::sql::analyzer::types::{TypedExpr, TypedExprKind};
use crate::sql::analyzer::{Analyzer, NullCatalog, Scope};
use crate::sql::error::SqlError;
use crate::sql::expr::compile::{compile_const_expr, compile_row_expr_for_table};
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::expr::typed_fold::fold_typed_expr;
use crate::sql::query_context::QueryContext;
use crate::sql::types::CastContext;
use anyhow::Result;

/// Evaluate a constant AST expression (no row context).
///
/// Uses task-local query context for compatibility call sites.
pub fn eval_const_ast_expr(expr: &sqlparser::ast::Expr) -> Result<Value> {
    let qctx = QueryContext::from_task_locals();
    let typed = compile_const_expr(expr, &qctx)?;
    eval_typed_expr(&typed, &Row::new(vec![]), &qctx)
}

/// Evaluate a SQL EXECUTE parameter expression with a target type.
///
/// This is the db9 equivalent of PostgreSQL's `EvaluateParams()`: each EXECUTE
/// parameter expression is analyzed to determine its natural type, then an
/// assignment-context cast is inserted when the type does not already match the
/// declared parameter type.  The coercion happens at the expression level
/// (before evaluation), not at the Value level, so:
///
/// - Bare string literals (`'1'`) are coercible to any target type — the cast
///   system converts them via the target type's input function.
/// - Invalid input (e.g. `'not json'` for a jsonb param) is rejected at
///   cast time with a proper `invalid input syntax` diagnostic.
///
/// This matches PostgreSQL's BIND-time behaviour where parameter values are
/// converted using the declared type's input function before execution begins.
pub fn eval_execute_param(
    expr: &sqlparser::ast::Expr,
    target_type: &DataType,
) -> Result<Value> {
    let qctx = QueryContext::from_task_locals();
    let catalog = NullCatalog;

    // Analyze the expression to determine its natural type.
    // We do NOT fold yet — folding happens after we insert the cast.
    let analyzed = Analyzer::analyze_expr_with_scope(&catalog, Scope::new(), expr)
        .map_err(SqlError::from)?;

    // If types already match, fold and evaluate directly.
    let coerced = if analyzed.data_type == *target_type {
        analyzed
    } else {
        // Wrap in an assignment-context cast, matching PostgreSQL's
        // EvaluateParams() which uses COERCION_ASSIGNMENT.  Let the cast
        // layer decide compatibility — do NOT open-code type checks here.
        TypedExpr::new(
            TypedExprKind::Cast {
                expr: Box::new(analyzed),
                target_type: target_type.clone(),
                cast_context: CastContext::Assignment,
            },
            target_type.clone(),
        )
    };

    let folded = fold_typed_expr(&coerced, &qctx);
    eval_typed_expr(&folded, &Row::new(vec![]), &qctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    fn parse_expr(sql: &str) -> sqlparser::ast::Expr {
        let sql = format!("SELECT {sql}");
        let ast = Parser::parse_sql(&PostgreSqlDialect {}, &sql).unwrap();
        let sqlparser::ast::Statement::Query(query) = ast.into_iter().next().unwrap() else {
            panic!("expected query");
        };
        let sqlparser::ast::SetExpr::Select(select) = *query.body else {
            panic!("expected select");
        };
        let Some(sqlparser::ast::SelectItem::UnnamedExpr(expr)) =
            select.projection.into_iter().next()
        else {
            panic!("expected expression projection");
        };
        expr
    }

    #[test]
    fn eval_execute_param_string_to_int() {
        let expr = parse_expr("'42'");
        let val = eval_execute_param(&expr, &DataType::Int32).unwrap();
        assert_eq!(val, Value::Int32(42));
    }

    #[test]
    fn eval_execute_param_string_to_jsonb() {
        let expr = parse_expr("'{\"a\":1}'");
        let val = eval_execute_param(&expr, &DataType::Jsonb).unwrap();
        assert!(matches!(val, Value::Jsonb(_)));
    }

    #[test]
    fn eval_execute_param_null_to_int() {
        let expr = parse_expr("NULL");
        let val = eval_execute_param(&expr, &DataType::Int32).unwrap();
        assert_eq!(val, Value::Null);
    }

    #[test]
    fn eval_execute_param_type_match_no_cast() {
        // The Analyzer produces Int64 for bare numeric literals like `42`.
        // When the target is also Int64, the types match and no Cast is inserted.
        let expr = parse_expr("42");
        let val = eval_execute_param(&expr, &DataType::Int64).unwrap();
        assert_eq!(val, Value::Int64(42));
    }

    #[test]
    fn eval_execute_param_bigint_to_int() {
        // Analyzer produces Int64 for `42`; target Int32 exercises the
        // assignment cast narrowing path (Int64 → Int32).
        let expr = parse_expr("42");
        let val = eval_execute_param(&expr, &DataType::Int32).unwrap();
        assert_eq!(val, Value::Int32(42));
    }

    #[test]
    fn eval_execute_param_invalid_jsonb_rejects() {
        let expr = parse_expr("'{bad}'");
        let err = eval_execute_param(&expr, &DataType::Jsonb).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("invalid input syntax") || msg.contains("jsonb"),
            "expected jsonb parse error, got: {msg}"
        );
    }

    #[test]
    fn eval_execute_param_invalid_int_rejects() {
        let expr = parse_expr("'not_a_number'");
        let err = eval_execute_param(&expr, &DataType::Int32).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("invalid input syntax") || msg.contains("int"),
            "expected int parse error, got: {msg}"
        );
    }

    #[test]
    fn eval_execute_param_string_to_uuid() {
        let expr = parse_expr("'550e8400-e29b-41d4-a716-446655440000'");
        let val = eval_execute_param(&expr, &DataType::Uuid).unwrap();
        assert!(matches!(val, Value::Uuid(_)));
    }
}

/// Evaluate an AST expression against a single-table row.
///
/// The scope is built from the table schema so column references resolve
/// to positional indices matching the row layout.
pub fn eval_ast_expr_with_row(
    expr: &sqlparser::ast::Expr,
    row: &Row,
    schema: &TableSchema,
    alias: &str,
) -> Result<Value> {
    let qctx = QueryContext::from_task_locals();
    let typed = compile_row_expr_for_table(expr, schema, alias, &qctx)?;
    eval_typed_expr(&typed, row, &qctx)
}
