use crate::model::TableSchema;
use crate::sql::analyzer::types::TypedExpr;
use crate::sql::analyzer::{Analyzer, Catalog, NullCatalog, Scope};
use crate::sql::error::SqlError;
use crate::sql::expr::typed_fold::fold_typed_expr;
use crate::sql::query_context::QueryContext;
use anyhow::Result;

/// Compile an AST expression that must not reference row columns.
pub fn compile_const_expr(expr: &sqlparser::ast::Expr, qctx: &QueryContext) -> Result<TypedExpr> {
    let catalog = NullCatalog;
    let typed =
        Analyzer::analyze_expr_with_scope(&catalog, Scope::new(), expr).map_err(SqlError::from)?;
    Ok(fold_typed_expr(&typed, qctx))
}

/// Compile an AST expression with a catalog for UDT resolution (e.g. enum casts in defaults).
pub fn compile_const_expr_with_catalog(
    expr: &sqlparser::ast::Expr,
    qctx: &QueryContext,
    catalog: &dyn Catalog,
) -> Result<TypedExpr> {
    let typed =
        Analyzer::analyze_expr_with_scope(catalog, Scope::new(), expr).map_err(SqlError::from)?;
    Ok(fold_typed_expr(&typed, qctx))
}

/// Compile an AST expression with table row scope.
pub fn compile_row_expr_for_table(
    expr: &sqlparser::ast::Expr,
    schema: &TableSchema,
    alias: &str,
    qctx: &QueryContext,
) -> Result<TypedExpr> {
    let analyzed = analyze_row_expr_for_table(expr, schema, alias)?;
    Ok(fold_typed_expr(&analyzed, qctx))
}

/// Analyze (but don't fold) an AST expression against a table's column scope.
///
/// Returns a `TypedExpr` with column references resolved and types inferred,
/// but without constant folding or `current_user` / `now()` substitution.
/// The caller must apply `fold_typed_expr` with a `QueryContext` before evaluation.
///
/// Useful for caching: the analyzed result depends only on schema (columns/types),
/// not on per-query context, so it can be cached across queries.
pub fn analyze_row_expr_for_table(
    expr: &sqlparser::ast::Expr,
    schema: &TableSchema,
    alias: &str,
) -> Result<TypedExpr> {
    let catalog = NullCatalog;
    let scope = Scope::from_table_schema(alias, schema);
    let typed = Analyzer::analyze_expr_with_scope(&catalog, scope, expr).map_err(SqlError::from)?;
    Ok(typed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DataType, UserTypeKind};
    use crate::sql::analyzer::catalog::MockCatalog;
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
    fn compile_const_expr_with_catalog_resolves_enum_cast() {
        let catalog = MockCatalog::builder()
            .user_defined_type(
                "public",
                "mood",
                UserTypeKind::Enum {
                    labels: vec!["happy".to_string(), "sad".to_string()],
                },
            )
            .build();
        let expr = parse_expr("'happy'::mood");
        let qctx = QueryContext::from_task_locals();

        let typed = compile_const_expr_with_catalog(&expr, &qctx, &catalog).unwrap();

        assert_eq!(
            typed.data_type,
            DataType::UserDefined("public.mood".to_string())
        );
    }

    #[test]
    fn compile_const_expr_without_catalog_rejects_enum_cast() {
        let expr = parse_expr("'happy'::mood");
        let qctx = QueryContext::from_task_locals();

        let err = compile_const_expr(&expr, &qctx).unwrap_err().to_string();

        assert!(err.contains("type \"mood\" does not exist"));
    }

    #[test]
    fn compile_const_expr_with_catalog_rejects_case_mismatched_quoted_udt() {
        let catalog = MockCatalog::builder()
            .user_defined_type(
                "public",
                "mood",
                UserTypeKind::Enum {
                    labels: vec!["happy".to_string(), "sad".to_string()],
                },
            )
            .build();
        let expr = parse_expr("'happy'::\"MOOD\"");
        let qctx = QueryContext::from_task_locals();

        let err = compile_const_expr_with_catalog(&expr, &qctx, &catalog)
            .unwrap_err()
            .to_string();

        assert!(err.contains("type \"MOOD\" does not exist"));
    }
}
