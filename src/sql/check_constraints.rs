use crate::model::{Row, TableSchema, Value};
use crate::sql::error::SqlError;
use crate::sql::expr::compile::compile_row_expr_for_table;
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::query_context::QueryContext;
use anyhow::{anyhow, Result};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

#[derive(Debug, Clone)]
pub struct CompiledCheckConstraint {
    pub name: Option<String>,
    pub expr_sql: String,
    pub expr: crate::sql::analyzer::types::TypedExpr,
}

pub fn compile_check_constraints(
    schema: &TableSchema,
    qctx: &QueryContext,
) -> Result<Vec<CompiledCheckConstraint>> {
    let dialect = PostgreSqlDialect {};
    let table_alias = schema.name.rsplit('.').next().unwrap_or(&schema.name);
    let mut compiled = Vec::with_capacity(schema.check_constraints.len());
    for check in &schema.check_constraints {
        let expr = Parser::new(&dialect)
            .try_with_sql(&check.expr)
            .and_then(|mut p| p.parse_expr())
            .map_err(|e| anyhow!("Invalid CHECK expression '{}': {}", check.expr, e))?;
        let typed = compile_row_expr_for_table(&expr, schema, table_alias, qctx)?;
        compiled.push(CompiledCheckConstraint {
            name: check.name.clone(),
            expr_sql: check.expr.clone(),
            expr: typed,
        });
    }
    Ok(compiled)
}

pub fn validate_compiled_check_constraints(
    schema: &TableSchema,
    checks: &[CompiledCheckConstraint],
    row: &Row,
    qctx: &QueryContext,
) -> Result<()> {
    for check in checks {
        let result = eval_typed_expr(&check.expr, row, qctx)?;
        match result {
            Value::Boolean(true) => {}
            Value::Boolean(false) => {
                let name = check
                    .name
                    .as_ref()
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| format!("({})", check.expr_sql));
                let short_table = schema.name.rsplit('.').next().unwrap_or(&schema.name);
                let row_str = row
                    .values
                    .iter()
                    .map(|v| match v {
                        Value::Null => "null".to_string(),
                        _ => format!("{}", v),
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(SqlError::CheckViolation {
                    table: short_table.to_string(),
                    constraint: name,
                    detail: row_str,
                }
                .into());
            }
            Value::Null => {}
            _ => {
                return Err(anyhow!(
                    "CHECK constraint must evaluate to boolean, got {:?}",
                    result
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CheckConstraint, ColumnDef, DataType};
    use std::sync::Arc;

    fn check_schema() -> TableSchema {
        TableSchema {
            name: "public.t_check".to_string(),
            table_id: 2,
            columns: vec![ColumnDef {
                name: "x".to_string(),
                data_type: DataType::Int32,
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            }],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![CheckConstraint {
                name: Some("x_positive".to_string()),
                expr: "x > 0".to_string(),
            }],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        }
    }

    #[test]
    fn compiled_check_constraints_validate_rows() {
        let schema = check_schema();
        let qctx = QueryContext::new(
            1,
            Arc::from("postgres"),
            Arc::from("postgres"),
            1_700_000_000_000,
            1_700_000_000_000,
            Arc::from("UTC"),
        );
        let checks = compile_check_constraints(&schema, &qctx).unwrap();

        validate_compiled_check_constraints(
            &schema,
            &checks,
            &Row::new(vec![Value::Int32(1)]),
            &qctx,
        )
        .unwrap();

        let err = validate_compiled_check_constraints(
            &schema,
            &checks,
            &Row::new(vec![Value::Int32(0)]),
            &qctx,
        )
        .unwrap_err();
        assert!(err.to_string().contains("x_positive"));

        // CHECK treats NULL as pass (unknown).
        validate_compiled_check_constraints(&schema, &checks, &Row::new(vec![Value::Null]), &qctx)
            .unwrap();
    }

    #[test]
    fn compile_check_constraints_reports_parse_error() {
        let mut schema = check_schema();
        schema.check_constraints[0].expr = "x >".to_string();

        let qctx = QueryContext::new(
            1,
            Arc::from("postgres"),
            Arc::from("postgres"),
            1_700_000_000_000,
            1_700_000_000_000,
            Arc::from("UTC"),
        );

        let err = compile_check_constraints(&schema, &qctx).unwrap_err();
        assert!(err.to_string().contains("Invalid CHECK expression"));
    }
}
