//! Default expression evaluation, `fill_missing_columns`, and `coerce_row_values`.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use tikv_client::Transaction;

use crate::model::{DataType, TableSchema, Value};
use crate::sql::error::SqlError;
use crate::sql::expr::compile::compile_const_expr;
use crate::sql::expr::static_eval::{eval_static_typed_expr, needs_async_materialization};
use crate::sql::expr::typed_rewrite::materialize_sequences_in_typed_expr;
use crate::sql::query_context::QueryContext;
use crate::sql::sequences::{self, set_lastval_sequence_name, LASTVAL_SENTINEL_KEY};
use crate::sql::value_coercion::coerce_value_for_column;
use crate::storage::TikvStore;

async fn eval_default_expr_maybe_sequence(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    sequence_values: &mut HashMap<String, i64>,
    search_path: &[String],
    expr_str: &str,
) -> Result<Value> {
    fn parse_default_expr(expr_str: &str) -> Result<sqlparser::ast::Expr> {
        let sql = format!("SELECT {}", expr_str);
        let dialect = PostgreSqlDialect {};
        let ast = Parser::parse_sql(&dialect, &sql)
            .map_err(|e| anyhow!("Failed to parse default expr: {}", e))?;
        if let Some(sqlparser::ast::Statement::Query(q)) = ast.into_iter().next() {
            if let sqlparser::ast::SetExpr::Select(s) = *q.body {
                if let Some(sqlparser::ast::SelectItem::UnnamedExpr(e)) =
                    s.projection.into_iter().next()
                {
                    return Ok(e);
                }
            }
        }
        Err(anyhow!("Failed to parse default expr: {}", expr_str))
    }

    let qctx = QueryContext::from_task_locals();
    let expr = parse_default_expr(expr_str)?;
    let typed = compile_const_expr(&expr, &qctx)?;
    if needs_async_materialization(&typed) {
        let materialized = materialize_sequences_in_typed_expr(
            store,
            txn,
            db_id,
            sequence_values,
            search_path,
            &typed,
            &qctx,
        )
        .await?;
        return eval_static_typed_expr(&materialized, &qctx);
    }
    eval_static_typed_expr(&typed, &qctx)
}

async fn eval_column_default_or_null_inner(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    sequence_values: &mut HashMap<String, i64>,
    search_path: &[String],
    schema: &TableSchema,
    column_idx: usize,
    sequence_defs: Option<&[crate::model::SequenceDef]>,
) -> Result<Value> {
    let column = schema
        .columns
        .get(column_idx)
        .ok_or_else(|| anyhow!("Column index {} out of bounds", column_idx))?;

    if column.is_serial {
        let (table_schema, table_name) = schema
            .name
            .rsplit_once('.')
            .unwrap_or(("public", schema.name.as_str()));

        let seq_full_name = match sequence_defs {
            Some(defs) => {
                match sequences::find_owned_sequence_full_name(defs, &schema.name, &column.name)? {
                    Some(full_name) => full_name,
                    None => format!(
                        "{}.{}",
                        table_schema,
                        sequences::implicit_sequence_name(table_name, &column.name)
                    ),
                }
            }
            None => {
                let defs = store.list_sequences(txn, db_id).await?;
                match sequences::find_owned_sequence_full_name(&defs, &schema.name, &column.name)? {
                    Some(full_name) => full_name,
                    None => format!(
                        "{}.{}",
                        table_schema,
                        sequences::implicit_sequence_name(table_name, &column.name)
                    ),
                }
            }
        };

        let seq_val = store.nextval_sequence(txn, db_id, &seq_full_name).await?;
        crate::sql::sequences::set_lastval(sequence_values, &seq_full_name, seq_val);
        sequence_values.insert(seq_full_name.clone(), seq_val);
        sequence_values.insert(LASTVAL_SENTINEL_KEY.to_string(), seq_val);
        set_lastval_sequence_name(sequence_values, &seq_full_name);
        return match column.data_type {
            DataType::Int64 => Ok(Value::Int64(seq_val)),
            _ => Ok(Value::Int32(seq_val.try_into().map_err(|_| {
                anyhow!(
                    "serial sequence value {} overflows INT4 for column \"{}\"",
                    seq_val,
                    column.name
                )
            })?)),
        };
    }

    if let Some(def) = &column.default_expr {
        return eval_default_expr_maybe_sequence(
            store,
            txn,
            db_id,
            sequence_values,
            search_path,
            def,
        )
        .await;
    }

    Ok(Value::Null)
}

pub async fn eval_column_default_or_null(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    sequence_values: &mut HashMap<String, i64>,
    search_path: &[String],
    schema: &TableSchema,
    column_idx: usize,
) -> Result<Value> {
    eval_column_default_or_null_inner(
        store,
        txn,
        db_id,
        sequence_values,
        search_path,
        schema,
        column_idx,
        None,
    )
    .await
}

pub async fn fill_missing_columns(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    sequence_values: &mut HashMap<String, i64>,
    search_path: &[String],
    schema: &TableSchema,
    row_vals: &mut [Value],
    indices: &[usize],
) -> Result<()> {
    let sequence_defs = if schema
        .columns
        .iter()
        .enumerate()
        .any(|(i, col)| col.is_serial && !indices.contains(&i))
    {
        Some(store.list_sequences(txn, db_id).await?)
    } else {
        None
    };

    for (i, _) in schema.columns.iter().enumerate() {
        if indices.contains(&i) {
            continue;
        }

        row_vals[i] = eval_column_default_or_null_inner(
            store,
            txn,
            db_id,
            sequence_values,
            search_path,
            schema,
            i,
            sequence_defs.as_deref(),
        )
        .await?;
    }
    Ok(())
}

/// Format a value for DETAIL messages matching PostgreSQL convention
/// (lowercase "null" instead of uppercase "NULL").
fn format_value_for_detail(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        _ => format!("{}", v),
    }
}

pub fn coerce_row_values(schema: &TableSchema, row_vals: &mut [Value]) -> Result<()> {
    for (i, c) in schema.columns.iter().enumerate() {
        let coerced = coerce_value_for_column(row_vals[i].clone(), c)?;
        if coerced == Value::Null && !c.nullable {
            let short_table = schema.name.rsplit('.').next().unwrap_or(&schema.name);
            let row_str = row_vals
                .iter()
                .map(format_value_for_detail)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(SqlError::NotNullViolation {
                column: c.name.clone(),
                relation: short_table.to_string(),
                message: format!(
                    "null value in column \"{}\" of relation \"{}\" violates not-null constraint\nDETAIL:  Failing row contains ({}).",
                    c.name, short_table, row_str
                ),
            }.into());
        }
        row_vals[i] = coerced;
    }
    Ok(())
}

pub fn coerce_row_values_allow_null(schema: &TableSchema, row_vals: &mut [Value]) -> Result<()> {
    for (i, c) in schema.columns.iter().enumerate() {
        row_vals[i] = coerce_value_for_column(row_vals[i].clone(), c)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ColumnDef;

    fn test_schema(nullable_second: bool) -> TableSchema {
        TableSchema {
            name: "public.t".to_string(),
            table_id: 1,
            columns: vec![
                ColumnDef {
                    name: "id".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: true,
                    unique: true,
                    is_serial: false,
                    default_expr: None,
                    collation: None,
                },
                ColumnDef {
                    name: "v".to_string(),
                    data_type: DataType::Text,
                    nullable: nullable_second,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    collation: None,
                },
            ],
            version: 1,
            pk_constraint_name: Some("t_pkey".to_string()),
            pk_indices: vec![0],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: "postgres".to_string(),
            from_alias: None,
        }
    }

    #[test]
    fn format_detail_value_renders_null_lowercase() {
        assert_eq!(format_value_for_detail(&Value::Null), "null");
        assert_eq!(
            format_value_for_detail(&Value::Text("abc".to_string())),
            "abc"
        );
    }

    #[test]
    fn coerce_row_values_applies_type_coercion() {
        let schema = test_schema(true);
        let mut row_vals = vec![Value::Text("7".to_string()), Value::Int32(9)];
        coerce_row_values(&schema, &mut row_vals).unwrap();
        assert_eq!(row_vals[0], Value::Int32(7));
        assert_eq!(row_vals[1], Value::Text("9".to_string()));
    }

    #[test]
    fn coerce_row_values_reports_not_null_violation_with_detail() {
        let schema = test_schema(false);
        let mut row_vals = vec![Value::Int32(1), Value::Null];
        let err = coerce_row_values(&schema, &mut row_vals)
            .unwrap_err()
            .to_string();
        assert!(err.contains("violates not-null constraint"));
        assert!(err.contains("Failing row contains (1, null)"));
    }

    #[test]
    fn coerce_row_values_allow_null_keeps_null() {
        let schema = test_schema(true);
        let mut row_vals = vec![Value::Text("1".to_string()), Value::Null];
        coerce_row_values_allow_null(&schema, &mut row_vals).unwrap();
        assert_eq!(row_vals[0], Value::Int32(1));
        assert_eq!(row_vals[1], Value::Null);
    }
}
