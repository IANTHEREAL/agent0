//! Trigger body execution (PL/pgSQL subset).

use super::rewrite::substitute_row_references;
use crate::model::{Row, TableSchema};
use crate::sql::executor::Executor;
use crate::sql::plpgsql::utils::plpgsql_outer_block_range;
use crate::sql::sequences::SequenceSession;
use anyhow::Result;
use tikv_client::Transaction;

pub(crate) async fn execute_trigger_body_standalone(
    executor: &Executor,
    txn: &mut Transaction,
    db_id: u64,
    sequence_values: &mut SequenceSession,
    schema: &TableSchema,
    body: &str,
    old_row: Option<&Row>,
    new_row: Option<&Row>,
    search_path: &[String],
) -> Result<()> {
    // This is intentionally a small subset of PL/pgSQL tailored for triggers:
    // - `NEW.col := <expr>` assignments
    // - `RETURN <...>` terminators
    // - Everything else is treated as a SQL statement and executed.
    //
    // It matches the existing BEFORE-trigger executor in `before.rs`, but adds
    // SQL statement execution for AFTER triggers.

    let mut new_values = match new_row {
        Some(r) => r.values.clone(),
        None => vec![crate::model::Value::Null; schema.columns.len()],
    };
    if new_values.len() < schema.columns.len() {
        new_values.resize(schema.columns.len(), crate::model::Value::Null);
    }

    let Some((begin_pos, end_pos)) = plpgsql_outer_block_range(body) else {
        return Ok(());
    };
    let block = &body[begin_pos..end_pos];

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

        let should_stop = {
            let stmt = stmt_buf.trim().trim_end_matches(';').trim();
            if stmt.is_empty() {
                false
            } else {
                execute_trigger_statement_standalone(
                    executor,
                    txn,
                    db_id,
                    sequence_values,
                    schema,
                    old_row,
                    &mut new_values,
                    stmt,
                    search_path,
                )
                .await?
            }
        };
        stmt_buf.clear();
        if should_stop {
            return Ok(());
        }
    }

    if !stmt_buf.trim().is_empty() {
        let stmt = stmt_buf.trim();
        let _ = execute_trigger_statement_standalone(
            executor,
            txn,
            db_id,
            sequence_values,
            schema,
            old_row,
            &mut new_values,
            stmt,
            search_path,
        )
        .await?;
    }

    Ok(())
}

pub(crate) async fn execute_trigger_statement_standalone(
    executor: &Executor,
    txn: &mut Transaction,
    db_id: u64,
    sequence_values: &mut SequenceSession,
    schema: &TableSchema,
    old_row: Option<&Row>,
    new_values: &mut [crate::model::Value],
    stmt: &str,
    search_path: &[String],
) -> Result<bool> {
    let stmt = stmt.trim().trim_end_matches(';').trim();
    let upper = stmt.to_uppercase();

    // PL/pgSQL no-op statement.
    if upper == "NULL" {
        return Ok(false);
    }
    if upper.starts_with("RETURN NEW") || upper.starts_with("RETURN OLD") || upper == "RETURN" {
        return Ok(true);
    }
    if upper.starts_with("RETURN NULL") {
        return Ok(true);
    }

    if upper.starts_with("NEW.") {
        if let Some((col, expr)) = parse_new_assignment(stmt) {
            if let Some(idx) = schema
                .columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(col))
            {
                let resolved_expr = substitute_row_references(expr, schema, new_values, old_row);
                let sql = format!("SELECT {}", resolved_expr);
                if let Ok(stmts) = crate::sql::parse_sql(&sql) {
                    if let Some(sqlparser::ast::Statement::Query(query)) = stmts.into_iter().next()
                    {
                        if let sqlparser::ast::SetExpr::Select(select) = *query.body {
                            if let Some(sqlparser::ast::SelectItem::UnnamedExpr(expr)) =
                                select.projection.into_iter().next()
                            {
                                let store = executor.store();
                                let value = if crate::sql::sequences::expr_needs_async_eval(&expr) {
                                    crate::sql::sequences::eval_expr_with_sequences(
                                        &store,
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

                                let coerced = crate::sql::value_coercion::coerce_value_for_column(
                                    value,
                                    &schema.columns[idx],
                                )?;
                                new_values[idx] = coerced;
                            }
                        }
                    }
                }
            }
        }
        return Ok(false);
    }

    // Treat as SQL statement.
    let substituted = substitute_row_references(stmt, schema, new_values, old_row);
    let statements = crate::sql::parse_sql(&substituted)?;
    let mut create_index_with_params =
        crate::sql::extract_create_index_with_params(&substituted).into_iter();
    for s in &statements {
        let with_params = if matches!(s, sqlparser::ast::Statement::CreateIndex { .. }) {
            create_index_with_params.next().flatten()
        } else {
            None
        };
        // Ignore result rows; errors propagate.
        let _ = executor
            .execute_statement_on_txn_with_create_index_with_params(
                txn,
                db_id,
                sequence_values,
                search_path,
                s,
                with_params.as_deref(),
                None,
                None,
                0,
            )
            .await?;
    }

    Ok(false)
}

fn parse_new_assignment(stmt: &str) -> Option<(&str, &str)> {
    // Accept both `:=` and `=` (common in test cases).
    let s = stmt.trim().trim_end_matches(';').trim();
    let rest = s.strip_prefix("NEW.").or_else(|| s.strip_prefix("new."))?;

    if let Some(pos) = rest.find(":=") {
        let col = rest[..pos].trim();
        let expr = rest[pos + 2..].trim();
        return Some((col, expr));
    }
    if let Some(pos) = rest.find('=') {
        // Skip `==`, `<=`, etc.
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
    use crate::model::{ColumnDef, DataType, Row, TableSchema, Value};
    use crate::sql::triggers::rewrite::value_to_sql_literal;

    use super::{parse_new_assignment, plpgsql_outer_block_range, substitute_row_references};

    #[test]
    fn parse_new_assignment_accepts_common_syntax() {
        assert_eq!(
            parse_new_assignment("NEW.updated_at := NOW();"),
            Some(("updated_at", "NOW()"))
        );
        assert_eq!(
            parse_new_assignment("new.updated_at = NOW();"),
            Some(("updated_at", "NOW()"))
        );
    }

    #[test]
    fn parse_new_assignment_rejects_comparisons() {
        assert_eq!(parse_new_assignment("NEW.a != 1"), None);
        assert_eq!(parse_new_assignment("NEW.a <= 1"), None);
        assert_eq!(parse_new_assignment("NEW.a >= 1"), None);
        assert_eq!(parse_new_assignment("NEW.a == 1"), None);
    }

    #[test]
    fn value_to_sql_literal_bytes_uses_single_backslash_x_prefix() {
        let value = Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(value_to_sql_literal(&value), "'\\xdeadbeef'");
    }

    #[test]
    fn substitute_row_references_does_not_prefix_match() {
        let schema = {
            let mut s = TableSchema::new(
                "test".to_string(),
                1,
                vec![
                    ColumnDef::new("id", DataType::Int32, false).primary_key(),
                    ColumnDef::new("id2", DataType::Int32, false),
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
    fn substitute_row_references_handles_schema_growth() {
        let schema = {
            let mut s = TableSchema::new(
                "test".to_string(),
                1,
                vec![
                    ColumnDef::new("id", DataType::Int32, false).primary_key(),
                    ColumnDef::new("id2", DataType::Int32, false),
                    ColumnDef::new("added", DataType::Int32, true),
                ],
                vec![0],
            );
            s.owner = String::new();
            s
        };

        // Simulate an async trigger event queued before `ALTER TABLE .. ADD COLUMN`.
        let new_values = vec![Value::Int32(7), Value::Int32(3)];
        let old_row = Row::new(vec![Value::Int32(9), Value::Int32(11)]);

        let result = substitute_row_references("NEW.id + NEW.id2", &schema, &new_values, None);
        assert_eq!(result, "7 + 3");

        let result = substitute_row_references("NEW.added IS NULL", &schema, &new_values, None);
        assert_eq!(result, "NULL IS NULL");

        let result =
            substitute_row_references("OLD.added IS NULL", &schema, &new_values, Some(&old_row));
        assert_eq!(result, "NULL IS NULL");
    }

    #[test]
    fn substitute_row_references_does_not_collide_a_aa() {
        let schema = {
            let mut s = TableSchema::new(
                "test".to_string(),
                1,
                vec![
                    ColumnDef::new("a", DataType::Int32, false),
                    ColumnDef::new("aa", DataType::Int32, false),
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
    fn plpgsql_outer_block_range_stops_at_end_semicolon() {
        let body = "DECLARE x INT;\nBEGIN\n  SELECT 1;\nEND;\n-- end\nSELECT 2;";
        let (start, end) = plpgsql_outer_block_range(body).expect("expected BEGIN..END;");
        let block = &body[start..end];
        assert!(block.contains("SELECT 1;"));
        assert!(!block.contains("END;"));
        assert!(!block.contains("-- end"));
        assert!(!block.contains("SELECT 2"));
    }

    #[test]
    fn plpgsql_outer_block_range_handles_nested_blocks() {
        let body = "BEGIN\n  BEGIN\n    SELECT 1;\n  END;\n  SELECT 2;\nEND;\n-- end";
        let (start, end) = plpgsql_outer_block_range(body).expect("expected BEGIN..END;");
        let block = &body[start..end];
        assert!(block.contains("BEGIN"));
        assert!(block.contains("END;"));
        assert!(block.contains("SELECT 2;"));
        assert!(!block.contains("-- end"));
    }

    #[test]
    fn plpgsql_outer_block_range_does_not_stop_at_case_end_semicolon() {
        let body =
            "BEGIN\n  NEW.col := CASE WHEN 1=1 THEN 2 ELSE 3 END;\n  SELECT 2;\nEND;\n-- end";
        let (start, end) = plpgsql_outer_block_range(body).expect("expected BEGIN..END;");
        let block = &body[start..end];
        assert!(block.contains("NEW.col := CASE"));
        assert!(block.contains("END;"));
        assert!(block.contains("SELECT 2;"));
        assert!(!block.contains("-- end"));
    }
}
