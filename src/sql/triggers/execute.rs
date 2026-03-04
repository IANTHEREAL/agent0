//! Trigger body execution (PL/pgSQL subset).

use super::rewrite::substitute_row_references;
use crate::model::{Row, TableSchema};
use crate::sql::executor::Executor;
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
            )
            .await?;
    }

    Ok(false)
}

pub(crate) fn plpgsql_outer_block_range(body: &str) -> Option<(usize, usize)> {
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    enum BlockKind {
        Begin,
        Case,
    }

    let bytes = body.as_bytes();
    let mut i = 0usize;
    let mut stack: Vec<BlockKind> = Vec::new();
    let mut block_start: Option<usize> = None;

    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let mut in_single_quote = false;
    let mut in_double_quote = false;

    while i < bytes.len() {
        if in_line_comment {
            if bytes[i] == b'\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }
        if in_block_comment {
            if bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                in_block_comment = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if in_single_quote {
            if bytes[i] == b'\'' {
                // SQL escapes single quotes by doubling them.
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                in_single_quote = false;
            }
            i += 1;
            continue;
        }
        if in_double_quote {
            if bytes[i] == b'"' {
                // SQL escapes double quotes by doubling them.
                if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                    i += 2;
                    continue;
                }
                in_double_quote = false;
            }
            i += 1;
            continue;
        }

        // Enter comments/strings.
        if bytes[i] == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
            in_line_comment = true;
            i += 2;
            continue;
        }
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            in_block_comment = true;
            i += 2;
            continue;
        }
        if bytes[i] == b'\'' {
            in_single_quote = true;
            i += 1;
            continue;
        }
        if bytes[i] == b'"' {
            in_double_quote = true;
            i += 1;
            continue;
        }

        // Skip dollar-quoted strings ($$...$$ or $tag$...$tag$).
        if bytes[i] == b'$' {
            if let Some(tag_end) = bytes[i + 1..].iter().position(|b| *b == b'$') {
                let tag_end = i + 1 + tag_end;
                let tag = &body[i + 1..tag_end];
                if tag.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                    let delim = &body[i..=tag_end];
                    if let Some(close_pos) = body[tag_end + 1..].find(delim) {
                        i = tag_end + 1 + close_pos + delim.len();
                        continue;
                    }
                }
            }
        }

        // Scan identifier-like tokens so `end2` doesn't match `END`.
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let start = i;
            i += 1;
            while i < bytes.len() {
                let b = bytes[i];
                if b.is_ascii_alphanumeric() || b == b'_' || b == b'$' {
                    i += 1;
                } else {
                    break;
                }
            }
            let token = &body[start..i];
            if token.eq_ignore_ascii_case("BEGIN") {
                if stack.is_empty() {
                    block_start = Some(i);
                }
                stack.push(BlockKind::Begin);
                continue;
            }
            if token.eq_ignore_ascii_case("CASE") {
                if !stack.is_empty() {
                    stack.push(BlockKind::Case);
                }
                continue;
            }
            if token.eq_ignore_ascii_case("END") && !stack.is_empty() {
                let mut j = i;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }

                // Handle `END <kw>;` forms like `END IF;`, `END LOOP;`, `END CASE;`.
                let mut next_token_end = j;
                let mut next_token: Option<&str> = None;
                if next_token_end < bytes.len()
                    && (bytes[next_token_end].is_ascii_alphabetic()
                        || bytes[next_token_end] == b'_')
                {
                    let next_start = next_token_end;
                    next_token_end += 1;
                    while next_token_end < bytes.len() {
                        let b = bytes[next_token_end];
                        if b.is_ascii_alphanumeric() || b == b'_' || b == b'$' {
                            next_token_end += 1;
                        } else {
                            break;
                        }
                    }
                    next_token = Some(&body[next_start..next_token_end]);
                }

                if let Some(next) = next_token {
                    if next.eq_ignore_ascii_case("IF") || next.eq_ignore_ascii_case("LOOP") {
                        continue;
                    }
                    if next.eq_ignore_ascii_case("CASE") {
                        if matches!(stack.last(), Some(BlockKind::Case)) {
                            stack.pop();
                        }
                        // Skip the `CASE` token so it won't be treated as a new `CASE`.
                        i = next_token_end;
                        continue;
                    }
                }

                // `END` closes an innermost SQL `CASE ... END` expression even when it is followed
                // by `;` (e.g. `NEW.col := CASE ... END;`).
                if matches!(stack.last(), Some(BlockKind::Case)) {
                    stack.pop();
                    continue;
                }

                // `END;` (or `END <label>;`) closes a `BEGIN ... END` block.
                if matches!(stack.last(), Some(BlockKind::Begin)) {
                    if j < bytes.len() && bytes[j] == b';' {
                        stack.pop();
                        if stack.is_empty() {
                            return Some((block_start.unwrap_or(i), start));
                        }
                        continue;
                    }

                    if next_token.is_some() {
                        let mut k = next_token_end;
                        while k < bytes.len() && bytes[k].is_ascii_whitespace() {
                            k += 1;
                        }
                        if k < bytes.len() && bytes[k] == b';' {
                            stack.pop();
                            if stack.is_empty() {
                                return Some((block_start.unwrap_or(i), start));
                            }
                            continue;
                        }
                    }
                }
                continue;
            }

            continue;
        }

        i += 1;
    }

    block_start.map(|start| (start, body.len()))
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
        let schema = TableSchema {
            name: "test".to_string(),
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
                    collation: None,
                },
                ColumnDef {
                    name: "id2".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    collation: None,
                },
            ],
            version: 1,
            pk_constraint_name: Some("test_pkey".to_string()),
            pk_indices: vec![0],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
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
        let schema = TableSchema {
            name: "test".to_string(),
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
                    collation: None,
                },
                ColumnDef {
                    name: "id2".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    collation: None,
                },
                ColumnDef {
                    name: "added".to_string(),
                    data_type: DataType::Int32,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    collation: None,
                },
            ],
            version: 1,
            pk_constraint_name: Some("test_pkey".to_string()),
            pk_indices: vec![0],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
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
        let schema = TableSchema {
            name: "test".to_string(),
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
                    collation: None,
                },
                ColumnDef {
                    name: "aa".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
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
