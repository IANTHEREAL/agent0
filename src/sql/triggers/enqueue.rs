use super::execute::{execute_trigger_body_standalone, plpgsql_outer_block_range};
use super::queue::TriggerOp;
use super::rewrite::substitute_row_references;
use crate::model::{Row, TriggerDef};
use crate::sql::executor::{Executor, PendingAsyncTrigger};
use crate::sql::sequences::SequenceSession;
use crate::storage::TikvStore;
use anyhow::Result;
use std::sync::Arc;
use tikv_client::Transaction;

const ASYNC_TRIGGER_KEYWORDS: &[&str] = &[
    "http_get",
    "http_post",
    "http_put",
    "http_delete",
    "http_request",
    "extensions.http",
];

fn trigger_body_needs_async(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    ASYNC_TRIGGER_KEYWORDS.iter().any(|kw| lower.contains(kw))
}

pub(crate) async fn enqueue_after_triggers(
    txn: &mut Transaction,
    db_id: u64,
    keyspace: &str,
    table_full_name: &str,
    op: TriggerOp,
    old_row: Option<&Row>,
    new_row: Option<&Row>,
    triggers: &[TriggerDef],
    store: &Arc<TikvStore>,
    executor: &Executor,
    sequence_values: &mut SequenceSession,
    search_path: &[String],
) -> Result<()> {
    let op_str = match op {
        TriggerOp::Insert => "INSERT",
        TriggerOp::Update => "UPDATE",
        TriggerOp::Delete => "DELETE",
    };

    let after_triggers: Vec<&TriggerDef> = triggers
        .iter()
        .filter(|t| {
            t.timing.eq_ignore_ascii_case("AFTER")
                && t.events.iter().any(|e| e.eq_ignore_ascii_case(op_str))
        })
        .collect();

    if after_triggers.is_empty() {
        return Ok(());
    }

    let schema = store.get_schema(txn, db_id, table_full_name).await?;
    let Some(schema) = schema else {
        return Ok(());
    };

    for trigger in after_triggers {
        let func = store.get_function(txn, db_id, &trigger.function).await?;
        let Some(func) = func else {
            continue;
        };

        if trigger_body_needs_async(&func.body) && crate::worker::get_system_store().is_some() {
            if let Some(sql) = flatten_trigger_body_to_sql(&func.body, &schema, new_row, old_row) {
                executor.push_pending_async_trigger(PendingAsyncTrigger {
                    keyspace: keyspace.to_string(),
                    db_id,
                    command: sql,
                });
            }
        } else {
            Box::pin(execute_trigger_body_standalone(
                executor,
                txn,
                db_id,
                sequence_values,
                &schema,
                &func.body,
                old_row,
                new_row,
                search_path,
            ))
            .await?;
        }
    }

    Ok(())
}

pub(crate) fn flatten_trigger_body_to_sql(
    body: &str,
    schema: &crate::model::TableSchema,
    new_row: Option<&Row>,
    old_row: Option<&Row>,
) -> Option<String> {
    let mut new_values = match new_row {
        Some(r) => r.values.clone(),
        None => vec![crate::model::Value::Null; schema.columns.len()],
    };
    if new_values.len() < schema.columns.len() {
        new_values.resize(schema.columns.len(), crate::model::Value::Null);
    }

    let (begin_pos, end_pos) = plpgsql_outer_block_range(body)?;
    let block = &body[begin_pos..end_pos];

    let mut statements = Vec::new();
    let mut stmt_buf = String::new();

    for raw_line in block.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with("--") {
            continue;
        }

        if !stmt_buf.is_empty() {
            stmt_buf.push(' ');
        }
        stmt_buf.push_str(line);

        if !line.ends_with(';') {
            continue;
        }

        if let Some(sql) = flatten_trigger_statement(&stmt_buf, schema, &new_values, old_row) {
            statements.push(sql);
        }
        stmt_buf.clear();
    }

    if !stmt_buf.trim().is_empty() {
        if let Some(sql) = flatten_trigger_statement(&stmt_buf, schema, &new_values, old_row) {
            statements.push(sql);
        }
    }

    if statements.is_empty() {
        None
    } else {
        Some(statements.join("; "))
    }
}

fn flatten_trigger_statement(
    stmt: &str,
    schema: &crate::model::TableSchema,
    new_values: &[crate::model::Value],
    old_row: Option<&Row>,
) -> Option<String> {
    let stmt = stmt.trim().trim_end_matches(';').trim();
    if stmt.is_empty() {
        return None;
    }

    let upper = stmt.to_ascii_uppercase();
    if upper == "NULL" || upper == "RETURN" || upper.starts_with("RETURN ") {
        return None;
    }

    if parse_new_assignment(stmt).is_some() {
        return None;
    }

    let substituted = substitute_row_references(stmt, schema, new_values, old_row);
    if let Some(rest) = strip_prefix_ignore_ascii_case(&substituted, "PERFORM ") {
        let rest = rest.trim();
        if rest.is_empty() {
            None
        } else {
            Some(format!("SELECT {}", rest))
        }
    } else {
        Some(substituted)
    }
}

fn strip_prefix_ignore_ascii_case<'a>(input: &'a str, prefix: &str) -> Option<&'a str> {
    if input.len() < prefix.len() {
        return None;
    }
    let (head, tail) = input.split_at(prefix.len());
    if head.eq_ignore_ascii_case(prefix) {
        Some(tail)
    } else {
        None
    }
}

fn parse_new_assignment(stmt: &str) -> Option<(&str, &str)> {
    let s = stmt.trim().trim_end_matches(';').trim();
    let rest = s.strip_prefix("NEW.").or_else(|| s.strip_prefix("new."))?;

    if let Some(pos) = rest.find(":=") {
        let col = rest[..pos].trim();
        let expr = rest[pos + 2..].trim();
        return Some((col, expr));
    }
    if let Some(pos) = rest.find('=') {
        let before = rest.as_bytes().get(pos.wrapping_sub(1)).copied();
        let after = rest.as_bytes().get(pos + 1).copied();
        if before == Some(b':')
            || before == Some(b'<')
            || before == Some(b'>')
            || before == Some(b'!')
            || after == Some(b'=')
        {
            return None;
        }
        let col = rest[..pos].trim();
        let expr = rest[pos + 1..].trim();
        return Some((col, expr));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ColumnDef, DataType, TableSchema, Value};

    fn test_schema() -> TableSchema {
        TableSchema {
            name: "t".to_string(),
            table_id: 1,
            columns: vec![
                ColumnDef {
                    name: "id".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: true,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    generation_expr: None,
                    generation_expr_authorized_by: None,
                    collation: None,
                },
                ColumnDef {
                    name: "name".to_string(),
                    data_type: DataType::Text,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    generation_expr: None,
                    generation_expr_authorized_by: None,
                    collation: None,
                },
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![0],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        }
    }

    #[test]
    fn async_keyword_detection_is_case_insensitive() {
        assert!(trigger_body_needs_async("PERFORM http_get('x')"));
        assert!(trigger_body_needs_async("select EXTENSIONS.HTTP('/x')"));
        assert!(!trigger_body_needs_async("PERFORM now()"));
    }

    #[test]
    fn parse_new_assignment_handles_assignment_syntaxes_and_ignores_comparisons() {
        assert_eq!(parse_new_assignment("NEW.id := 1"), Some(("id", "1")));
        assert_eq!(
            parse_new_assignment("new.name = 'x'"),
            Some(("name", "'x'"))
        );

        assert_eq!(parse_new_assignment("NEW.id >= 1"), None);
        assert_eq!(parse_new_assignment("NEW.id != 1"), None);
        assert_eq!(parse_new_assignment("id = 1"), None);
    }

    #[test]
    fn strip_prefix_ignore_ascii_case_works() {
        assert_eq!(
            strip_prefix_ignore_ascii_case("PeRfOrM 1", "PERFORM "),
            Some("1")
        );
        assert!(strip_prefix_ignore_ascii_case("SELECT 1", "PERFORM ").is_none());
    }

    #[test]
    fn flatten_trigger_statement_covers_core_paths() {
        let schema = test_schema();
        let new_values = vec![Value::Int32(7), Value::Text("neo".to_string())];
        let old_row = crate::model::Row::new(vec![Value::Int32(1), Value::Text("old".to_string())]);

        assert_eq!(
            flatten_trigger_statement("RETURN NEW;", &schema, &new_values, Some(&old_row)),
            None
        );
        assert_eq!(
            flatten_trigger_statement("NULL;", &schema, &new_values, Some(&old_row)),
            None
        );
        assert_eq!(
            flatten_trigger_statement("NEW.name := 'x';", &schema, &new_values, Some(&old_row)),
            None
        );

        let sql = flatten_trigger_statement("PERFORM 1 + 2;", &schema, &new_values, Some(&old_row))
            .expect("perform should rewrite");
        assert_eq!(sql, "SELECT 1 + 2");

        let sql = flatten_trigger_statement(
            "SELECT NEW.id, NEW.name;",
            &schema,
            &new_values,
            Some(&old_row),
        )
        .expect("statement should be preserved");
        assert!(sql.contains("7"));
        assert!(sql.contains("neo"));
    }

    #[test]
    fn flatten_trigger_body_to_sql_joins_statements_and_skips_non_sql() {
        let schema = test_schema();
        let new_row =
            crate::model::Row::new(vec![Value::Int32(42), Value::Text("alice".to_string())]);
        let old_row =
            crate::model::Row::new(vec![Value::Int32(41), Value::Text("bob".to_string())]);

        let body = r#"
BEGIN
  -- comment
  PERFORM 1;
  NEW.name := 'x';
  SELECT NEW.id, OLD.name;
  RETURN NEW;
END;
"#;

        let flattened = flatten_trigger_body_to_sql(body, &schema, Some(&new_row), Some(&old_row))
            .expect("should produce sql");
        assert!(flattened.contains("SELECT 1"));
        assert!(flattened.contains("42"));
        assert!(flattened.contains("bob"));
        assert!(!flattened.to_ascii_uppercase().contains("RETURN NEW"));
    }
}
