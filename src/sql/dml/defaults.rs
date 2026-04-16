//! Default expression evaluation, `fill_missing_columns`, and `coerce_row_values`.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use tikv_client::Transaction;

use crate::model::{DataType, TableSchema, UserTypeDef, Value};
use crate::sql::analyzer::CatalogSnapshot;
use crate::sql::error::SqlError;
use crate::sql::expr::compile::{compile_const_expr, compile_const_expr_with_catalog};
use crate::sql::expr::static_eval::{eval_static_typed_expr, needs_async_materialization};
use crate::sql::expr::typed_rewrite::materialize_sequences_in_typed_expr;
use crate::sql::query_context::QueryContext;
use crate::sql::sequences;
use crate::sql::sequences::{classify_serial_default, SequenceSession, SerialDefaultBehavior};
use crate::sql::types::cast::coerce_value_for_column;
use crate::storage::TikvStore;

/// Build a `CatalogSnapshot` populated with all UDTs in the database so that
/// default expressions containing enum casts (e.g. `'happy'::mood`) can be
/// resolved by the analyzer.
async fn build_udt_catalog(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
) -> Result<CatalogSnapshot> {
    let mut catalog = CatalogSnapshot::new(search_path.to_vec(), db_id);
    let types: Vec<UserTypeDef> = store.list_types(txn, db_id).await?;
    for udt in types {
        catalog.add_schema(&udt.schema);
        let full_name = format!("{}.{}", udt.schema, udt.name);
        catalog.add_type(&full_name, udt);
    }
    Ok(catalog)
}

fn data_type_has_custom_type(data_type: &sqlparser::ast::DataType) -> bool {
    match data_type {
        sqlparser::ast::DataType::Custom(..) => true,
        sqlparser::ast::DataType::Array(inner) => match inner {
            sqlparser::ast::ArrayElemTypeDef::AngleBracket(inner)
            | sqlparser::ast::ArrayElemTypeDef::SquareBracket(inner) => {
                data_type_has_custom_type(inner)
            }
            sqlparser::ast::ArrayElemTypeDef::None => false,
        },
        _ => false,
    }
}

/// Returns true if the parsed expression contains a `DataType::Custom` cast
/// that would require catalog lookup (e.g. `'happy'::mood`).
fn expr_has_custom_type_cast(expr: &sqlparser::ast::Expr) -> bool {
    use core::ops::ControlFlow;
    use sqlparser::ast::{visit_expressions, Expr};

    let mut found = false;
    let _ = visit_expressions(expr, |e| {
        if found {
            return ControlFlow::Break(());
        }

        match e {
            Expr::Cast { data_type, .. }
            | Expr::TryCast { data_type, .. }
            | Expr::SafeCast { data_type, .. }
                if data_type_has_custom_type(data_type) =>
            {
                found = true;
                return ControlFlow::Break(());
            }
            _ => {}
        }

        ControlFlow::<()>::Continue(())
    });
    found
}

async fn eval_default_expr_maybe_sequence(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    sequence_values: &mut SequenceSession,
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

    // If the expression contains a custom type cast (e.g. 'happy'::mood),
    // build a catalog with UDTs so the analyzer can resolve enum types.
    let typed = if expr_has_custom_type_cast(&expr) {
        let catalog = build_udt_catalog(store, txn, db_id, search_path).await?;
        compile_const_expr_with_catalog(&expr, &qctx, &catalog)?
    } else {
        compile_const_expr(&expr, &qctx)?
    };

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
    sequence_values: &mut SequenceSession,
    search_path: &[String],
    schema: &TableSchema,
    column_idx: usize,
    sequence_defs: Option<&[crate::model::SequenceDef]>,
) -> Result<Value> {
    let column = schema
        .columns
        .get(column_idx)
        .ok_or_else(|| anyhow!("Column index {} out of bounds", column_idx))?;

    // Dropped columns always get NULL — their metadata flags (is_serial, etc.)
    // may be stale; skip any default/sequence evaluation.
    if column.is_dropped {
        return Ok(Value::Null);
    }

    if column.is_serial {
        match classify_serial_default(column.default_expr.as_deref()) {
            SerialDefaultBehavior::ImplicitSequence => {
                let (table_schema, table_name) = schema
                    .name
                    .rsplit_once('.')
                    .unwrap_or(("public", schema.name.as_str()));

                let seq_full_name = match sequence_defs {
                    Some(defs) => {
                        match sequences::find_owned_sequence_full_name(
                            defs,
                            &schema.name,
                            &column.name,
                        )? {
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
                        match sequences::find_owned_sequence_full_name(
                            &defs,
                            &schema.name,
                            &column.name,
                        )? {
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
                sequence_values.record_nextval(seq_full_name, seq_val);
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
            SerialDefaultBehavior::ExplicitExpr(_) => {
                // Fall through to default_expr evaluation below.
            }
            SerialDefaultBehavior::ExplicitNull => {
                return Ok(Value::Null);
            }
        }
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
    sequence_values: &mut SequenceSession,
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
    sequence_values: &mut SequenceSession,
    search_path: &[String],
    schema: &TableSchema,
    row_vals: &mut [Value],
    indices: &[usize],
) -> Result<()> {
    let sequence_defs = if schema.columns.iter().enumerate().any(|(i, col)| {
        col.is_serial
            && !indices.contains(&i)
            && matches!(
                classify_serial_default(col.default_expr.as_deref()),
                SerialDefaultBehavior::ImplicitSequence,
            )
    }) {
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

    fn test_schema(nullable_second: bool) -> TableSchema {
        TableSchema::new(
            "public.t".to_string(),
            1,
            vec![
                ColumnDef::new("id", DataType::Int32, false)
                    .primary_key()
                    .unique(),
                ColumnDef::new("v", DataType::Text, nullable_second),
            ],
            vec![0],
        )
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

    #[test]
    fn expr_has_custom_type_cast_detects_enum_cast() {
        assert!(expr_has_custom_type_cast(&parse_expr("'happy'::mood")));
    }

    #[test]
    fn expr_has_custom_type_cast_detects_enum_array_cast() {
        assert!(expr_has_custom_type_cast(&parse_expr(
            "ARRAY['happy']::mood[]"
        )));
    }

    #[test]
    fn expr_has_custom_type_cast_detects_nested_enum_cast() {
        assert!(expr_has_custom_type_cast(&parse_expr(
            "coalesce('happy'::mood, 'sad'::mood)"
        )));
    }

    #[test]
    fn expr_has_custom_type_cast_ignores_builtin_cast() {
        assert!(!expr_has_custom_type_cast(&parse_expr("'happy'::text")));
    }
}
