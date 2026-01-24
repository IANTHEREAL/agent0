//! BEFORE trigger execution for INSERT/UPDATE DML operations.

use crate::storage::TikvStore;
use crate::types::{Row, TableSchema, TriggerDef, Value};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tikv_client::Transaction;

pub async fn apply_before_triggers(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    sequence_values: &mut HashMap<String, i64>,
    search_path: &[String],
    triggers: &[TriggerDef],
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
        let func_def = match store.get_function(txn, db_id, &trigger.function).await? {
            Some(f) => f,
            None => continue,
        };

        let result = execute_trigger_function(
            store,
            txn,
            db_id,
            sequence_values,
            search_path,
            &func_def,
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
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    sequence_values: &mut HashMap<String, i64>,
    search_path: &[String],
    func_def: &crate::types::FunctionDef,
    schema: &TableSchema,
    new_row: &Row,
    old_row: Option<&Row>,
) -> Result<TriggerResult> {
    let lang = func_def.language.to_lowercase();
    if lang != "plpgsql" && lang != "sql" {
        return Ok(TriggerResult::Unchanged);
    }

    execute_trigger_body(
        store,
        txn,
        db_id,
        sequence_values,
        search_path,
        &func_def.body,
        schema,
        new_row,
        old_row,
    )
    .await
}

async fn execute_trigger_body(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    sequence_values: &mut HashMap<String, i64>,
    search_path: &[String],
    body: &str,
    schema: &TableSchema,
    new_row: &Row,
    old_row: Option<&Row>,
) -> Result<TriggerResult> {
    use super::expr::eval_expr;
    use super::sequences;

    let mut modified_values = new_row.values.clone();
    let mut was_modified = false;

    let body_upper = body.to_uppercase();
    let begin_pos = match body_upper.find("BEGIN") {
        Some(p) => p + 5,
        None => return Ok(TriggerResult::Unchanged),
    };
    let end_pos = body_upper.rfind("END").unwrap_or(body.len());
    let block_content = &body[begin_pos..end_pos];

    for line in block_content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("--") {
            continue;
        }

        let line_upper = line.to_uppercase();

        if line_upper.starts_with("RETURN NEW") {
            return if was_modified {
                Ok(TriggerResult::Row(Row::new(modified_values)))
            } else {
                Ok(TriggerResult::Row(new_row.clone()))
            };
        }

        if line_upper.starts_with("RETURN NULL") {
            return Ok(TriggerResult::Null);
        }

        if line_upper.starts_with("RETURN OLD") {
            return match old_row {
                Some(old) => Ok(TriggerResult::Row(old.clone())),
                None => Ok(TriggerResult::Null),
            };
        }

        if line_upper.starts_with("NEW.") {
            let assignment = &line[4..];

            let (col_name, expr_str) = if let Some(pos) = assignment.find(":=") {
                (
                    assignment[..pos].trim(),
                    assignment[pos + 2..].trim().trim_end_matches(';'),
                )
            } else if let Some(pos) = assignment.find('=') {
                let before = assignment.chars().nth(pos.saturating_sub(1));
                let after = assignment.chars().nth(pos + 1);
                if before == Some(':')
                    || before == Some('<')
                    || before == Some('>')
                    || before == Some('!')
                    || after == Some('=')
                {
                    continue;
                }
                (
                    assignment[..pos].trim(),
                    assignment[pos + 1..].trim().trim_end_matches(';'),
                )
            } else {
                continue;
            };

            let col_idx = schema
                .columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(col_name));

            if let Some(idx) = col_idx {
                let resolved_expr =
                    substitute_row_references(expr_str, schema, &modified_values, old_row);

                let sql = format!("SELECT {}", resolved_expr);
                if let Ok(stmts) = super::parse_sql(&sql) {
                    if let Some(sqlparser::ast::Statement::Query(query)) = stmts.into_iter().next()
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
                                    eval_expr(&expr, None, None)?
                                };

                                let coerced = super::helpers::coerce_value_for_column(
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
    }

    if was_modified {
        Ok(TriggerResult::Row(Row::new(modified_values)))
    } else {
        Ok(TriggerResult::Unchanged)
    }
}

fn substitute_row_references(
    expr: &str,
    schema: &TableSchema,
    new_values: &[Value],
    old_row: Option<&Row>,
) -> String {
    let mut result = expr.to_string();

    for (idx, col) in schema.columns.iter().enumerate() {
        let patterns = [
            format!("NEW.{}", col.name.to_uppercase()),
            format!("new.{}", col.name.to_lowercase()),
            format!("NEW.{}", col.name),
            format!("new.{}", col.name),
        ];
        let value_str = value_to_sql_literal(&new_values[idx]);

        for pattern in &patterns {
            if result.to_uppercase().contains(&pattern.to_uppercase()) {
                result = case_insensitive_replace(&result, pattern, &value_str);
            }
        }
    }

    if let Some(old) = old_row {
        for (idx, col) in schema.columns.iter().enumerate() {
            let patterns = [
                format!("OLD.{}", col.name.to_uppercase()),
                format!("old.{}", col.name.to_lowercase()),
                format!("OLD.{}", col.name),
                format!("old.{}", col.name),
            ];
            let value_str = value_to_sql_literal(&old.values[idx]);

            for pattern in &patterns {
                if result.to_uppercase().contains(&pattern.to_uppercase()) {
                    result = case_insensitive_replace(&result, pattern, &value_str);
                }
            }
        }
    }

    result
}

fn case_insensitive_replace(s: &str, pattern: &str, replacement: &str) -> String {
    let s_upper = s.to_uppercase();
    let pattern_upper = pattern.to_uppercase();

    let mut result = String::new();
    let mut last_end = 0;

    for (start, _) in s_upper.match_indices(&pattern_upper) {
        result.push_str(&s[last_end..start]);
        result.push_str(replacement);
        last_end = start + pattern.len();
    }
    result.push_str(&s[last_end..]);

    result
}

fn value_to_sql_literal(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Boolean(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
        Value::Int32(n) => n.to_string(),
        Value::Int64(n) => n.to_string(),
        Value::Float64(f) => f.to_string(),
        Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
        Value::Timestamp(ts) => {
            let secs = ts / 1000;
            let millis = ts % 1000;
            let datetime = chrono::DateTime::from_timestamp(secs, (millis * 1_000_000) as u32)
                .unwrap_or_else(|| chrono::DateTime::UNIX_EPOCH);
            format!("'{}'", datetime.format("%Y-%m-%d %H:%M:%S%.3f"))
        }
        Value::Date(days) => match crate::types::date::format_date_days(*days) {
            Ok(s) => format!("'{}'", s),
            Err(_) => format!("'{}'", days),
        },
        Value::Uuid(bytes) => {
            let u = uuid::Uuid::from_bytes(*bytes);
            format!("'{}'", u)
        }
        Value::Bytes(b) => format!("'\\x{}'", hex::encode(b)),
        Value::Json(s) | Value::Jsonb(s) => format!("'{}'", s.replace('\'', "''")),
        Value::Array(arr) => {
            let items: Vec<String> = arr.iter().map(value_to_sql_literal).collect();
            format!("ARRAY[{}]", items.join(","))
        }
        Value::Vector(v) => {
            let items: Vec<String> = v.iter().map(|f| f.to_string()).collect();
            format!("[{}]", items.join(","))
        }
        Value::Interval(iv) => format!("INTERVAL '{}'", iv),
        Value::Time(micros) => {
            let total_secs = micros / 1_000_000;
            let hours = total_secs / 3600;
            let mins = (total_secs % 3600) / 60;
            let secs = total_secs % 60;
            format!("'{:02}:{:02}:{:02}'", hours, mins, secs)
        }
        Value::Numeric(d) => d.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_substitute_row_references() {
        let schema = TableSchema {
            name: "test".to_string(),
            table_id: 1,
            columns: vec![
                crate::types::ColumnDef {
                    name: "id".to_string(),
                    data_type: crate::types::DataType::Int32,
                    nullable: false,
                    primary_key: true,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
                crate::types::ColumnDef {
                    name: "updated_at".to_string(),
                    data_type: crate::types::DataType::Timestamp,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
            ],
            version: 1,
            pk_constraint_name: Some("test_pkey".to_string()),
            pk_indices: vec![0],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
        };

        let new_values = vec![Value::Int32(1), Value::Null];

        let result = substitute_row_references("NEW.id + 1", &schema, &new_values, None);
        assert_eq!(result, "1 + 1");

        let result = substitute_row_references("NOW()", &schema, &new_values, None);
        assert_eq!(result, "NOW()");
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
