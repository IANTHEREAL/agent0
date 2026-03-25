//! BEFORE trigger execution for INSERT/UPDATE DML operations.

use super::cache::{TriggerBodyCache, TriggerStatement};
use super::rewrite::substitute_row_references;
use crate::model::{FunctionDef, Row, TableSchema, TriggerDef};
use crate::sql::sequences::SequenceSession;
use crate::storage::TikvStore;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tikv_client::Transaction;

pub async fn prefetch_trigger_functions(
    trigger_cache: &TriggerBodyCache,
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    triggers: &[TriggerDef],
    event: &str,
) -> Result<HashMap<String, FunctionDef>> {
    let mut func_cache = HashMap::new();
    let before_triggers: Vec<&TriggerDef> = triggers
        .iter()
        .filter(|t| {
            t.timing.eq_ignore_ascii_case("BEFORE")
                && t.events.iter().any(|e| e.eq_ignore_ascii_case(event))
        })
        .collect();

    for trigger in before_triggers {
        if func_cache.contains_key(&trigger.function) {
            continue;
        }
        if let Some(func_def) = store.get_function(txn, db_id, &trigger.function).await? {
            if let Ok(compiled) = trigger_cache.get_or_compile(db_id, func_def.oid, &func_def.body)
            {
                let _ = compiled;
            }
            func_cache.insert(trigger.function.clone(), func_def);
        }
    }

    Ok(func_cache)
}

pub async fn apply_before_triggers_with_cache(
    trigger_cache: &TriggerBodyCache,
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    sequence_values: &mut SequenceSession,
    search_path: &[String],
    triggers: &[TriggerDef],
    func_cache: &HashMap<String, FunctionDef>,
    schema: &TableSchema,
    event: &str,
    new_row: Row,
    old_row: Option<&Row>,
) -> Result<Option<Row>> {
    let before_triggers: Vec<&TriggerDef> = triggers
        .iter()
        .filter(|t| {
            t.timing.eq_ignore_ascii_case("BEFORE")
                && t.events.iter().any(|e| e.eq_ignore_ascii_case(event))
        })
        .collect();

    if before_triggers.is_empty() {
        return Ok(Some(new_row));
    }

    let mut current_row = new_row;

    for trigger in before_triggers {
        let func_def = match func_cache.get(&trigger.function) {
            Some(f) => f,
            None => continue,
        };

        let result = execute_trigger_function(
            trigger_cache,
            store,
            txn,
            db_id,
            sequence_values,
            search_path,
            func_def,
            schema,
            &current_row,
            old_row,
        )
        .await?;

        match result {
            TriggerResult::Row(modified_row) => {
                current_row = modified_row;
            }
            TriggerResult::Null => {
                return Ok(None);
            }
            TriggerResult::Unchanged => {}
        }
    }

    Ok(Some(current_row))
}

enum TriggerResult {
    Row(Row),
    Null,
    Unchanged,
}

async fn execute_trigger_function(
    trigger_cache: &TriggerBodyCache,
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    sequence_values: &mut SequenceSession,
    search_path: &[String],
    func_def: &crate::model::FunctionDef,
    schema: &TableSchema,
    new_row: &Row,
    old_row: Option<&Row>,
) -> Result<TriggerResult> {
    let lang = func_def.language.to_lowercase();
    if lang != "plpgsql" && lang != "sql" {
        return Ok(TriggerResult::Unchanged);
    }

    execute_trigger_body_cached(
        trigger_cache,
        store,
        txn,
        db_id,
        sequence_values,
        search_path,
        func_def.oid,
        &func_def.body,
        schema,
        new_row,
        old_row,
    )
    .await
}

async fn execute_trigger_body_cached(
    trigger_cache: &TriggerBodyCache,
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    sequence_values: &mut SequenceSession,
    search_path: &[String],
    func_oid: u32,
    body: &str,
    schema: &TableSchema,
    new_row: &Row,
    old_row: Option<&Row>,
) -> Result<TriggerResult> {
    use crate::sql::sequences;

    let compiled = trigger_cache.get_or_compile(db_id, func_oid, body)?;

    if compiled.statements.is_empty() {
        return Ok(TriggerResult::Unchanged);
    }

    let mut modified_values = new_row.values.clone();
    let mut was_modified = false;

    for stmt in &compiled.statements {
        match stmt {
            TriggerStatement::ReturnNew => {
                return if was_modified {
                    Ok(TriggerResult::Row(Row::new(modified_values)))
                } else {
                    Ok(TriggerResult::Row(new_row.clone()))
                };
            }
            TriggerStatement::ReturnNull => {
                return Ok(TriggerResult::Null);
            }
            TriggerStatement::ReturnOld => {
                return match old_row {
                    Some(old) => Ok(TriggerResult::Row(old.clone())),
                    None => Ok(TriggerResult::Null),
                };
            }
            TriggerStatement::Assignment { column, expr_str } => {
                let col_idx = schema
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(column));

                if let Some(idx) = col_idx {
                    let resolved_expr =
                        substitute_row_references(expr_str, schema, &modified_values, old_row);

                    let sql = format!("SELECT {}", resolved_expr);
                    if let Ok(stmts) = crate::sql::parse_sql(&sql) {
                        if let Some(sqlparser::ast::Statement::Query(query)) =
                            stmts.into_iter().next()
                        {
                            if let sqlparser::ast::SetExpr::Select(select) = *query.body {
                                if let Some(sqlparser::ast::SelectItem::UnnamedExpr(expr)) =
                                    select.projection.into_iter().next()
                                {
                                    let value = if sequences::expr_needs_async_eval(&expr) {
                                        sequences::eval_expr_with_sequences(
                                            store,
                                            txn,
                                            db_id,
                                            sequence_values,
                                            search_path,
                                            &expr,
                                            None,
                                            None,
                                        )
                                        .await?
                                    } else {
                                        crate::sql::expr::bridge::eval_const_ast_expr(&expr)?
                                    };

                                    let coerced =
                                        crate::sql::value_coercion::coerce_value_for_column(
                                            value,
                                            &schema.columns[idx],
                                        )?;

                                    modified_values[idx] = coerced;
                                    was_modified = true;
                                }
                            }
                        }
                    }
                }
            }
            TriggerStatement::Skip => {}
        }
    }

    if was_modified {
        Ok(TriggerResult::Row(Row::new(modified_values)))
    } else {
        Ok(TriggerResult::Unchanged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Value;
    use crate::sql::triggers::rewrite::value_to_sql_literal;

    #[test]
    fn test_substitute_row_references() {
        let schema = {
            let mut s = TableSchema::new(
                "test".to_string(),
                1,
                vec![
                    crate::model::ColumnDef::new("id", crate::model::DataType::Int32, false)
                        .primary_key(),
                    crate::model::ColumnDef::new(
                        "updated_at",
                        crate::model::DataType::Timestamp,
                        true,
                    ),
                ],
                vec![0],
            );
            s.owner = String::new();
            s
        };

        let new_values = vec![Value::Int32(1), Value::Null];

        let result = substitute_row_references("NEW.id + 1", &schema, &new_values, None);
        assert_eq!(result, "1 + 1");

        let result = substitute_row_references("NOW()", &schema, &new_values, None);
        assert_eq!(result, "NOW()");
    }

    #[test]
    fn test_substitute_row_references_does_not_prefix_match() {
        let schema = {
            let mut s = TableSchema::new(
                "test".to_string(),
                1,
                vec![
                    crate::model::ColumnDef::new("id", crate::model::DataType::Int32, false)
                        .primary_key(),
                    crate::model::ColumnDef::new("id2", crate::model::DataType::Int32, false),
                ],
                vec![0],
            );
            s.owner = String::new();
            s
        };

        let new_values = vec![Value::Int32(7), Value::Int32(3)];
        let old_row = Row::new(vec![Value::Int32(9), Value::Int32(11)]);

        let result = substitute_row_references("NEW.id2 + 1", &schema, &new_values, None);
        assert_eq!(result, "3 + 1");

        let result = substitute_row_references("NEW.id + NEW.id2", &schema, &new_values, None);
        assert_eq!(result, "7 + 3");

        let result =
            substitute_row_references("OLD.id2 + OLD.id", &schema, &new_values, Some(&old_row));
        assert_eq!(result, "11 + 9");
    }

    #[test]
    fn test_substitute_row_references_does_not_collide_a_aa() {
        let schema = {
            let mut s = TableSchema::new(
                "test".to_string(),
                1,
                vec![
                    crate::model::ColumnDef::new("a", crate::model::DataType::Int32, false),
                    crate::model::ColumnDef::new("aa", crate::model::DataType::Int32, false),
                ],
                vec![],
            );
            s.owner = String::new();
            s
        };

        let new_values = vec![Value::Int32(1), Value::Int32(9)];
        let result = substitute_row_references("NEW.aa + 1", &schema, &new_values, None);
        assert_eq!(result, "9 + 1");
    }

    #[test]
    fn test_substitute_row_references_ignores_strings_and_comments() {
        let schema = {
            let mut s = TableSchema::new(
                "test".to_string(),
                1,
                vec![
                    crate::model::ColumnDef::new("a", crate::model::DataType::Int32, false),
                    crate::model::ColumnDef::new("aa", crate::model::DataType::Int32, false),
                ],
                vec![],
            );
            s.owner = String::new();
            s
        };

        let new_values = vec![Value::Int32(1), Value::Int32(9)];
        let expr = "'NEW.aa' || NEW.aa::TEXT /* NEW.aa */ -- NEW.aa";
        let result = substitute_row_references(expr, &schema, &new_values, None);
        assert_eq!(result, "'NEW.aa' || 9::TEXT /* NEW.aa */ -- NEW.aa");
    }

    #[test]
    fn test_value_to_sql_literal() {
        assert_eq!(value_to_sql_literal(&Value::Null), "NULL");
        assert_eq!(value_to_sql_literal(&Value::Boolean(true)), "TRUE");
        assert_eq!(value_to_sql_literal(&Value::Int32(42)), "42");
        assert_eq!(
            value_to_sql_literal(&Value::Text("hello".to_string())),
            "'hello'"
        );
        assert_eq!(
            value_to_sql_literal(&Value::Text("it's".to_string())),
            "'it''s'"
        );
    }
}
