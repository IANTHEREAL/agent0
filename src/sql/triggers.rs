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

    let body_upper = body.to_ascii_uppercase();
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
    let bytes = expr.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;

    while i < bytes.len() {
        // Line comment.
        if bytes[i] == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
            let start = i;
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            out.extend_from_slice(&bytes[start..i]);
            continue;
        }

        // Block comment (supports nesting).
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            let start = i;
            i += 2;
            let mut depth = 1usize;
            while i < bytes.len() && depth > 0 {
                if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                    depth += 1;
                    i += 2;
                    continue;
                }
                if bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                    depth = depth.saturating_sub(1);
                    i += 2;
                    continue;
                }
                i += 1;
            }
            out.extend_from_slice(&bytes[start..i]);
            continue;
        }

        // Single-quoted string literal.
        if bytes[i] == b'\'' {
            let start = i;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    // Escaped quote: ''.
                    if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.extend_from_slice(&bytes[start..i]);
            continue;
        }

        // Double-quoted identifier.
        if bytes[i] == b'"' {
            let start = i;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'"' {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.extend_from_slice(&bytes[start..i]);
            continue;
        }

        // Dollar-quoted strings ($tag$...$tag$ or $$...$$).
        if bytes[i] == b'$' {
            // Skip parameter placeholders like $1.
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 {
                out.extend_from_slice(&bytes[i..j]);
                i = j;
                continue;
            }

            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != b'$' {
                if bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_' {
                    j += 1;
                    continue;
                }
                break;
            }
            if j < bytes.len() && bytes[j] == b'$' {
                let start = i;
                let delim = &bytes[i..=j];
                let delim_len = delim.len();
                i = j + 1;
                while i + delim_len <= bytes.len() {
                    if &bytes[i..i + delim_len] == delim {
                        i += delim_len;
                        break;
                    }
                    i += 1;
                }
                out.extend_from_slice(&bytes[start..i]);
                continue;
            }

            out.push(bytes[i]);
            i += 1;
            continue;
        }

        // Identifier token.
        if is_ident_start(bytes[i]) {
            let start = i;
            i += 1;
            while i < bytes.len() && is_ident_continue(bytes[i]) {
                i += 1;
            }

            let ident = &bytes[start..i];
            let is_new = ident.eq_ignore_ascii_case(b"NEW");
            let is_old = ident.eq_ignore_ascii_case(b"OLD");
            if (is_new || is_old) && i < bytes.len() && bytes[i] == b'.' {
                let col_start = i + 1;
                if col_start < bytes.len() && is_ident_start(bytes[col_start]) {
                    let mut col_end = col_start + 1;
                    while col_end < bytes.len() && is_ident_continue(bytes[col_end]) {
                        col_end += 1;
                    }
                    if let Ok(col_name) = std::str::from_utf8(&bytes[col_start..col_end]) {
                        if let Some(idx) = schema
                            .columns
                            .iter()
                            .position(|c| c.name.eq_ignore_ascii_case(col_name))
                        {
                            if is_new {
                                let value = new_values
                                    .get(idx)
                                    .map(value_to_sql_literal)
                                    .unwrap_or_else(|| "NULL".to_string());
                                out.extend_from_slice(value.as_bytes());
                                i = col_end;
                                continue;
                            }

                            if let Some(old) = old_row {
                                let value = old
                                    .values
                                    .get(idx)
                                    .map(value_to_sql_literal)
                                    .unwrap_or_else(|| "NULL".to_string());
                                out.extend_from_slice(value.as_bytes());
                                i = col_end;
                                continue;
                            }
                        }
                    }
                }
            }

            out.extend_from_slice(ident);
            continue;
        }

        out.push(bytes[i]);
        i += 1;
    }

    String::from_utf8(out).unwrap_or_else(|_| expr.to_string())
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
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
    fn test_substitute_row_references_does_not_prefix_match() {
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
                    name: "id2".to_string(),
                    data_type: crate::types::DataType::Int32,
                    nullable: false,
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

        let new_values = vec![Value::Int32(7), Value::Int32(3)];
        let old_row = Row::new(vec![Value::Int32(9), Value::Int32(11)]);

        let result = substitute_row_references("NEW.id2 + 1", &schema, &new_values, None);
        assert_eq!(result, "3 + 1");

        let result = substitute_row_references("NEW.id + NEW.id2", &schema, &new_values, None);
        assert_eq!(result, "7 + 3");

        let result = substitute_row_references("OLD.id2 + OLD.id", &schema, &new_values, Some(&old_row));
        assert_eq!(result, "11 + 9");
    }

    #[test]
    fn test_substitute_row_references_does_not_collide_a_aa() {
        let schema = TableSchema {
            name: "test".to_string(),
            table_id: 1,
            columns: vec![
                crate::types::ColumnDef {
                    name: "a".to_string(),
                    data_type: crate::types::DataType::Int32,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
                crate::types::ColumnDef {
                    name: "aa".to_string(),
                    data_type: crate::types::DataType::Int32,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
        };

        let new_values = vec![Value::Int32(1), Value::Int32(9)];
        let result = substitute_row_references("NEW.aa + 1", &schema, &new_values, None);
        assert_eq!(result, "9 + 1");
    }

    #[test]
    fn test_substitute_row_references_ignores_strings_and_comments() {
        let schema = TableSchema {
            name: "test".to_string(),
            table_id: 1,
            columns: vec![
                crate::types::ColumnDef {
                    name: "a".to_string(),
                    data_type: crate::types::DataType::Int32,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
                crate::types::ColumnDef {
                    name: "aa".to_string(),
                    data_type: crate::types::DataType::Int32,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
        };

        let new_values = vec![Value::Int32(1), Value::Int32(9)];
        let expr = "'NEW.aa' || NEW.aa::TEXT /* NEW.aa */ -- NEW.aa";
        let result = substitute_row_references(expr, &schema, &new_values, None);
        assert_eq!(
            result,
            "'NEW.aa' || 9::TEXT /* NEW.aa */ -- NEW.aa"
        );
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
