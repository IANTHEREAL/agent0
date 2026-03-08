use crate::model::{ColumnDef, TableSchema};
use crate::sql::analyzer::types::{FunctionKind, TypedExpr, TypedExprKind};
use crate::sql::error::SqlError;
use crate::sql::expr::compile::compile_row_expr_for_table;
use crate::sql::expr::traverse::visit_any;
use crate::sql::expr::typed_fold::is_volatile_or_side_effecting_builtin;
use crate::sql::query_context::QueryContext;
use crate::sql::types::cast::CastContext;
use crate::sql::types::coercion::is_assignment_compatible;
use anyhow::{anyhow, Result};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

#[derive(Debug, Clone)]
pub(crate) struct CompiledGeneratedColumn {
    pub(crate) column_idx: usize,
    pub(crate) expr: TypedExpr,
    pub(crate) uses_embedding: bool,
    pub(crate) embedding_authorized: bool,
}

pub(crate) fn compile_generated_columns(
    schema: &TableSchema,
    qctx: &QueryContext,
) -> Result<Vec<CompiledGeneratedColumn>> {
    let mut compiled = Vec::new();
    for column_idx in 0..schema.columns.len() {
        if let Some(col) = compile_generated_column(schema, column_idx, qctx)? {
            compiled.push(col);
        }
    }
    Ok(compiled)
}

pub(crate) fn compile_generated_column(
    schema: &TableSchema,
    column_idx: usize,
    qctx: &QueryContext,
) -> Result<Option<CompiledGeneratedColumn>> {
    let col = schema
        .columns
        .get(column_idx)
        .ok_or_else(|| anyhow!("generated column index {} out of bounds", column_idx))?;
    let Some(gen_expr_str) = col.generation_expr.as_deref() else {
        return Ok(None);
    };

    let expr = Parser::new(&PostgreSqlDialect {})
        .try_with_sql(gen_expr_str)
        .and_then(|mut p| p.parse_expr())
        .map_err(|e| {
            SqlError::SqlStructure(format!(
                "invalid generation expression for column \"{}\": {}",
                col.name, e
            ))
        })?;
    let table_alias = schema.name.rsplit('.').next().unwrap_or(&schema.name);
    let typed = compile_row_expr_for_table(&expr, schema, table_alias, qctx)?;
    let uses_embedding = validate_generation_expr_semantics(&typed, schema, column_idx, &col.name)?;
    let typed = coerce_generated_expr_to_column(typed, col)?;

    Ok(Some(CompiledGeneratedColumn {
        column_idx,
        expr: typed,
        uses_embedding,
        embedding_authorized: col.generation_expr_authorized_by.is_some(),
    }))
}

fn coerce_generated_expr_to_column(expr: TypedExpr, col: &ColumnDef) -> Result<TypedExpr> {
    if expr.data_type == col.data_type {
        return Ok(expr);
    }
    if expr.is_null_constant() {
        return Ok(TypedExpr::null(col.data_type.clone()));
    }
    if !is_assignment_compatible(&expr.data_type, &col.data_type) {
        return Err(SqlError::DataTypeMismatch {
            message: format!(
                "column \"{}\" is of type {} but generation expression is of type {}",
                col.name, col.data_type, expr.data_type
            ),
        }
        .into());
    }

    Ok(TypedExpr::new(
        TypedExprKind::Cast {
            expr: Box::new(expr),
            target_type: col.data_type.clone(),
            cast_context: CastContext::Assignment,
        },
        col.data_type.clone(),
    ))
}

fn is_non_immutable_generated_builtin(name: &str) -> bool {
    if name.eq_ignore_ascii_case("EMBED_TEXT") {
        // DB9_DIVERGENCE(#1626): allow EMBED_TEXT in STORED generated columns,
        // but only with explicit DDL-time authorization and runtime revalidation.
        return false;
    }

    if is_volatile_or_side_effecting_builtin(name) {
        return true;
    }

    matches!(
        name.to_ascii_uppercase().as_str(),
        "NOW"
            | "CURRENT_TIMESTAMP"
            | "STATEMENT_TIMESTAMP"
            | "TRANSACTION_TIMESTAMP"
            | "CURRENT_DATE"
            | "CURRENT_TIME"
            | "LOCALTIME"
            | "LOCALTIMESTAMP"
            | "CURRENT_USER"
            | "SESSION_USER"
            | "USER"
            | "CURRENT_DATABASE"
            | "VERSION"
            | "PG_BACKEND_PID"
            | "EMBEDDING"
            | "VEC_EMBED_COSINE_DISTANCE"
            | "VEC_EMBED_L2_DISTANCE"
            | "VEC_EMBED_INNER_PRODUCT"
    )
}

fn validate_generation_expr_semantics(
    typed: &TypedExpr,
    schema: &TableSchema,
    column_idx: usize,
    column_name: &str,
) -> Result<bool> {
    let mut uses_embedding = false;
    let mut violation: Option<String> = None;

    visit_any(typed, |node| match &node.kind {
        TypedExprKind::ScalarSubquery(_)
        | TypedExprKind::ArraySubquery(_)
        | TypedExprKind::Exists { .. }
        | TypedExprKind::InSubquery { .. }
        | TypedExprKind::TupleInSubquery { .. }
        | TypedExprKind::AnyAll { .. } => {
            violation = Some(format!(
                "generation expression for column \"{}\" must not use subqueries",
                column_name
            ));
            true
        }
        TypedExprKind::AggregateCall { .. } | TypedExprKind::WindowCall { .. } => {
            violation = Some(format!(
                "generation expression for column \"{}\" must not use aggregate or window functions",
                column_name
            ));
            true
        }
        TypedExprKind::Parameter { .. } | TypedExprKind::Default => {
            violation = Some(format!(
                "generation expression for column \"{}\" must not use parameters or DEFAULT",
                column_name
            ));
            true
        }
        TypedExprKind::ColumnRef {
            scope_depth,
            column_index,
            ..
        } => {
            if *scope_depth != 0 {
                violation = Some(format!(
                    "generation expression for column \"{}\" must not reference outer query columns",
                    column_name
                ));
                return true;
            }

            let Some(referenced) = schema.columns.get(*column_index) else {
                return false;
            };
            if *column_index == column_idx {
                violation = Some(format!(
                    "generation expression for column \"{}\" must not reference itself",
                    column_name
                ));
                return true;
            }
            if referenced.generation_expr.is_some() {
                violation = Some(format!(
                    "generation expression for column \"{}\" must not reference generated column \"{}\"",
                    column_name, referenced.name
                ));
                return true;
            }
            false
        }
        TypedExprKind::FunctionCall { func, .. } => match func.kind {
            FunctionKind::UserDefined { .. } => {
                violation = Some(format!(
                    "generation expression for column \"{}\" must not use user-defined function \"{}\"",
                    column_name, func.name
                ));
                true
            }
            FunctionKind::Builtin => {
                if func.name.eq_ignore_ascii_case("EMBED_TEXT") {
                    uses_embedding = true;
                    false
                } else if is_non_immutable_generated_builtin(&func.name) {
                    violation = Some(format!(
                        "generation expression for column \"{}\" uses non-immutable function \"{}\"",
                        column_name, func.name
                    ));
                    true
                } else {
                    false
                }
            }
        },
        _ => false,
    });

    if let Some(message) = violation {
        return Err(SqlError::SqlStructure(message).into());
    }

    Ok(uses_embedding)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DataType;
    use crate::sql::error::SqlError;

    fn generated_schema() -> TableSchema {
        TableSchema {
            name: "public.generated_test".to_string(),
            table_id: 1,
            columns: vec![
                ColumnDef {
                    name: "a".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    generation_expr: None,
                    generation_expr_authorized_by: None,
                    collation: None,
                },
                ColumnDef {
                    name: "b".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    generation_expr: Some("a + 1".to_string()),
                    generation_expr_authorized_by: Some("admin".to_string()),
                    collation: None,
                },
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        }
    }

    fn embedding_generated_schema(expr: &str, data_type: DataType) -> TableSchema {
        TableSchema {
            name: "public.generated_embedding_test".to_string(),
            table_id: 2,
            columns: vec![
                ColumnDef {
                    name: "body".to_string(),
                    data_type: DataType::Text,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    generation_expr: None,
                    generation_expr_authorized_by: None,
                    collation: None,
                },
                ColumnDef {
                    name: "body_vec".to_string(),
                    data_type,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    generation_expr: Some(expr.to_string()),
                    generation_expr_authorized_by: Some("admin".to_string()),
                    collation: None,
                },
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        }
    }

    #[test]
    fn compile_generated_columns_compiles_all_generated_columns_once() {
        let compiled =
            compile_generated_columns(&generated_schema(), &QueryContext::for_tests()).unwrap();
        assert_eq!(compiled.len(), 1);
        assert_eq!(compiled[0].column_idx, 1);
        assert!(compiled[0].embedding_authorized);
        assert!(!compiled[0].uses_embedding);
    }

    #[test]
    fn compile_generated_column_returns_none_for_regular_columns() {
        let compiled =
            compile_generated_column(&generated_schema(), 0, &QueryContext::for_tests()).unwrap();
        assert!(compiled.is_none());
    }

    #[test]
    fn compile_generated_column_rejects_dimension_mismatch_at_compile_time() {
        let err = compile_generated_column(
            &embedding_generated_schema(
                "EMBED_TEXT('tidbcloud_free/amazon/titan-embed-text-v2', body, '{\"dimensions\":768}')",
                DataType::Vector(1024),
            ),
            1,
            &QueryContext::for_tests(),
        )
        .unwrap_err();
        let sql = err.downcast_ref::<SqlError>().expect("must be SqlError");
        assert_eq!(sql.sqlstate(), "42804");
        assert!(sql
            .to_string()
            .contains("column \"body_vec\" is of type vector(1024) but generation expression is of type vector(768)"));
    }

    #[test]
    fn compile_generated_column_preserves_matching_embedding_dimensions() {
        let compiled = compile_generated_column(
            &embedding_generated_schema(
                "EMBED_TEXT('tidbcloud_free/amazon/titan-embed-text-v2', body, '{\"dimensions\":1024}')",
                DataType::Vector(1024),
            ),
            1,
            &QueryContext::for_tests(),
        )
        .unwrap()
        .expect("generated column should compile");
        assert!(compiled.uses_embedding);
        assert_eq!(compiled.expr.data_type, DataType::Vector(1024));
    }

    #[test]
    fn compile_generated_column_rejects_unknown_vector_dimension_for_concrete_target() {
        let err = compile_generated_column(
            &embedding_generated_schema(
                "EMBED_TEXT('tidbcloud_free/amazon/titan-embed-text-v2', body)",
                DataType::Vector(1024),
            ),
            1,
            &QueryContext::for_tests(),
        )
        .unwrap_err();
        let sql = err.downcast_ref::<SqlError>().expect("must be SqlError");
        assert_eq!(sql.sqlstate(), "42804");
        assert!(sql
            .to_string()
            .contains("column \"body_vec\" is of type vector(1024) but generation expression is of type vector"));
    }

    #[test]
    fn compile_generated_column_rejects_invalid_embed_text_options_for_bare_vector() {
        let err = compile_generated_column(
            &embedding_generated_schema(
                "EMBED_TEXT('tidbcloud_free/amazon/titan-embed-text-v2', body, '{\"dimensions\":\"1024\"}')",
                DataType::Vector(0),
            ),
            1,
            &QueryContext::for_tests(),
        )
        .unwrap_err();
        let sql = err.downcast_ref::<SqlError>().expect("must be SqlError");
        assert_eq!(sql.sqlstate(), "22023");
        assert!(sql
            .to_string()
            .contains("embed_text: dimensions must be a number"));
    }

    #[test]
    fn compile_generated_column_rejects_invalid_casted_embed_text_options_for_bare_vector() {
        let err = compile_generated_column(
            &embedding_generated_schema(
                "EMBED_TEXT('tidbcloud_free/amazon/titan-embed-text-v2', body, 123::text)",
                DataType::Vector(0),
            ),
            1,
            &QueryContext::for_tests(),
        )
        .unwrap_err();
        let sql = err.downcast_ref::<SqlError>().expect("must be SqlError");
        assert_eq!(sql.sqlstate(), "22023");
        assert!(sql
            .to_string()
            .contains("embed_text: JSON options must be an object"));
    }
}
