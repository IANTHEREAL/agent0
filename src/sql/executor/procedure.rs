use super::super::ddl;
use super::super::names;
use super::super::value_coercion::infer_data_type;
use super::super::{parse_sql, ExecuteResult, Session};
use super::core::Executor;
use crate::types::{ColumnDef, DataType, Row, TableSchema, Value};
use anyhow::{anyhow, Result};
use sqlparser::ast::{Ident, ObjectName, Query, Statement};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::tokenizer::{Token, Tokenizer};
use std::collections::HashMap;
use tikv_client::Transaction;

fn object_name_from_token(token: &str) -> Result<ObjectName> {
    let token = token.trim().trim_end_matches(';');
    if token.is_empty() {
        return Err(anyhow!("Missing object name"));
    }
    let parts: Vec<&str> = token.split('.').collect();
    match parts.as_slice() {
        [name] if !name.is_empty() => Ok(ObjectName(vec![sqlparser::ast::Ident::new(*name)])),
        [schema, name] if !schema.is_empty() && !name.is_empty() => Ok(ObjectName(vec![
            sqlparser::ast::Ident::new(*schema),
            sqlparser::ast::Ident::new(*name),
        ])),
        _ => Err(anyhow!("Invalid object name '{}'", token)),
    }
}

fn tokenize_non_whitespace(sql: &str) -> Result<Vec<Token>> {
    let dialect = PostgreSqlDialect {};
    let mut tokenizer = Tokenizer::new(&dialect, sql);
    let tokens = tokenizer
        .tokenize()
        .map_err(|e| anyhow!("SQL tokenize error: {}", e))?;
    Ok(tokens
        .into_iter()
        .filter(|t| !matches!(t, Token::Whitespace(_)))
        .collect())
}

fn is_unquoted_keyword(token: &Token, keyword: &str) -> bool {
    match token {
        Token::Word(w) => w.quote_style.is_none() && w.value.eq_ignore_ascii_case(keyword),
        _ => false,
    }
}

fn escape_sql_string(s: &str, quote_char: char) -> String {
    s.replace(quote_char, &format!("{}{}", quote_char, quote_char))
}

fn escape_postgresql_escaped_string(s: &str) -> String {
    // PostgreSQL E'...' strings need both backslashes and quotes escaped
    // Backslash: \ -> \\
    // Quote: ' -> ''
    s.replace('\\', "\\\\").replace('\'', "''")
}

fn parse_call_arguments(args_str: &str) -> Result<Vec<String>> {
    if args_str.trim().is_empty() {
        return Ok(vec![]);
    }

    let dialect = PostgreSqlDialect {};
    let mut tokenizer = Tokenizer::new(&dialect, args_str);
    let tokens = tokenizer
        .tokenize()
        .map_err(|e| anyhow!("Failed to tokenize CALL arguments: {}", e))?;

    let mut args: Vec<String> = Vec::new();
    let mut current_arg = String::new();
    let mut depth = 0usize;

    for token in tokens {
        match token {
            Token::Comma if depth == 0 => {
                args.push(current_arg.trim().to_string());
                current_arg.clear();
            }
            Token::LParen => {
                depth += 1;
                current_arg.push('(');
            }
            Token::RParen => {
                if depth > 0 {
                    depth -= 1;
                }
                current_arg.push(')');
            }
            Token::LBracket => {
                depth += 1;
                current_arg.push('[');
            }
            Token::RBracket => {
                if depth > 0 {
                    depth -= 1;
                }
                current_arg.push(']');
            }
            Token::LBrace => {
                depth += 1;
                current_arg.push('{');
            }
            Token::RBrace => {
                if depth > 0 {
                    depth -= 1;
                }
                current_arg.push('}');
            }
            Token::Whitespace(ws) => {
                current_arg.push_str(&ws.to_string());
            }
            Token::Word(w) => {
                if let Some(q) = w.quote_style {
                    current_arg.push(q);
                    current_arg.push_str(&escape_sql_string(&w.value, q));
                    current_arg.push(q);
                } else {
                    current_arg.push_str(&w.value);
                }
            }
            Token::Number(n, _) => {
                current_arg.push_str(&n);
            }
            Token::SingleQuotedString(s) => {
                current_arg.push('\'');
                current_arg.push_str(&escape_sql_string(&s, '\''));
                current_arg.push('\'');
            }
            Token::DoubleQuotedString(s) => {
                current_arg.push('"');
                current_arg.push_str(&escape_sql_string(&s, '"'));
                current_arg.push('"');
            }
            Token::NationalStringLiteral(s) => {
                current_arg.push_str("N'");
                current_arg.push_str(&escape_sql_string(&s, '\''));
                current_arg.push('\'');
            }
            Token::HexStringLiteral(s) => {
                current_arg.push_str("X'");
                current_arg.push_str(&escape_sql_string(&s, '\''));
                current_arg.push('\'');
            }
            Token::EscapedStringLiteral(s) => {
                current_arg.push_str("E'");
                current_arg.push_str(&escape_postgresql_escaped_string(&s));
                current_arg.push('\'');
            }
            Token::Placeholder(s) => {
                current_arg.push_str(&s);
            }
            _ => {
                current_arg.push_str(&token.to_string());
            }
        }
    }

    if !current_arg.trim().is_empty() {
        args.push(current_arg.trim().to_string());
    }

    Ok(args)
}

fn substitute_parameters_in_statement(
    stmt_str: &str,
    param_map: &HashMap<String, (String, String)>,
) -> Result<String> {
    let dialect = PostgreSqlDialect {};
    let mut tokenizer = Tokenizer::new(&dialect, stmt_str);
    let tokens = tokenizer
        .tokenize()
        .map_err(|e| anyhow!("Failed to tokenize statement: {}", e))?;

    let mut result = String::new();

    for token in tokens {
        match &token {
            Token::Word(w) if w.quote_style.is_none() => {
                if let Some((value, data_type)) = param_map.get(&w.value) {
                    let dt_lower = data_type.to_lowercase();
                    let formatted_value = if dt_lower.contains("int")
                        || dt_lower.contains("float")
                        || dt_lower.contains("real")
                        || dt_lower.contains("numeric")
                        || dt_lower.contains("decimal")
                        || dt_lower.contains("double")
                    {
                        value.clone()
                    } else if value.starts_with('\'') && value.ends_with('\'') {
                        value.clone()
                    } else {
                        format!("'{}'", value)
                    };
                    result.push_str(&formatted_value);
                } else {
                    result.push_str(&w.value);
                }
            }
            Token::Word(w) => {
                // Quoted identifier - must re-escape
                if let Some(q) = w.quote_style {
                    result.push(q);
                    result.push_str(&escape_sql_string(&w.value, q));
                    result.push(q);
                } else {
                    result.push_str(&w.value);
                }
            }
            Token::SingleQuotedString(s) => {
                result.push('\'');
                result.push_str(&escape_sql_string(s, '\''));
                result.push('\'');
            }
            Token::DoubleQuotedString(s) => {
                result.push('"');
                result.push_str(&escape_sql_string(s, '"'));
                result.push('"');
            }
            Token::NationalStringLiteral(s) => {
                result.push_str("N'");
                result.push_str(&escape_sql_string(s, '\''));
                result.push('\'');
            }
            Token::HexStringLiteral(s) => {
                result.push_str("X'");
                result.push_str(&escape_sql_string(s, '\''));
                result.push('\'');
            }
            Token::EscapedStringLiteral(s) => {
                result.push_str("E'");
                result.push_str(&escape_postgresql_escaped_string(s));
                result.push('\'');
            }
            _ => {
                result.push_str(&token.to_string());
            }
        }
    }

    Ok(result)
}

fn parse_object_name(tokens: &[Token]) -> Result<(ObjectName, usize)> {
    let mut parts: Vec<Ident> = Vec::new();
    let mut i = 0usize;

    let Token::Word(w) = tokens
        .get(i)
        .ok_or_else(|| anyhow!("Missing object name"))?
    else {
        return Err(anyhow!("Missing object name"));
    };
    parts.push(Ident {
        value: w.value.clone(),
        quote_style: w.quote_style,
    });
    i += 1;

    if matches!(tokens.get(i), Some(Token::Period)) {
        i += 1;
        let Token::Word(w) = tokens
            .get(i)
            .ok_or_else(|| anyhow!("Invalid object name"))?
        else {
            return Err(anyhow!("Invalid object name"));
        };
        parts.push(Ident {
            value: w.value.clone(),
            quote_style: w.quote_style,
        });
        i += 1;
    }

    if matches!(tokens.get(i), Some(Token::Period)) {
        return Err(anyhow!("Invalid object name"));
    }

    Ok((ObjectName(parts), i))
}

fn parse_refresh_materialized_view_name(sql: &str) -> Result<ObjectName> {
    let tokens = tokenize_non_whitespace(sql)?;
    let mut i = 0usize;

    if !is_unquoted_keyword(
        tokens
            .get(i)
            .ok_or_else(|| anyhow!("Invalid REFRESH MATERIALIZED VIEW syntax"))?,
        "REFRESH",
    ) || !is_unquoted_keyword(
        tokens
            .get(i + 1)
            .ok_or_else(|| anyhow!("Invalid REFRESH MATERIALIZED VIEW syntax"))?,
        "MATERIALIZED",
    ) || !is_unquoted_keyword(
        tokens
            .get(i + 2)
            .ok_or_else(|| anyhow!("Invalid REFRESH MATERIALIZED VIEW syntax"))?,
        "VIEW",
    ) {
        return Err(anyhow!("Invalid REFRESH MATERIALIZED VIEW syntax"));
    }
    i += 3;

    if tokens
        .get(i)
        .is_some_and(|t| is_unquoted_keyword(t, "CONCURRENTLY"))
    {
        i += 1;
    }

    if !matches!(tokens.get(i), Some(Token::Word(_))) {
        return Err(anyhow!("Missing view name"));
    }
    let (name, _) = parse_object_name(tokens.get(i..).unwrap_or_default())?;
    Ok(name)
}

fn parse_drop_materialized_view(sql: &str) -> Result<(Vec<ObjectName>, bool, bool)> {
    let tokens = tokenize_non_whitespace(sql)?;
    let mut i = 0usize;

    if !is_unquoted_keyword(
        tokens
            .get(i)
            .ok_or_else(|| anyhow!("Invalid DROP MATERIALIZED VIEW syntax"))?,
        "DROP",
    ) || !is_unquoted_keyword(
        tokens
            .get(i + 1)
            .ok_or_else(|| anyhow!("Invalid DROP MATERIALIZED VIEW syntax"))?,
        "MATERIALIZED",
    ) || !is_unquoted_keyword(
        tokens
            .get(i + 2)
            .ok_or_else(|| anyhow!("Invalid DROP MATERIALIZED VIEW syntax"))?,
        "VIEW",
    ) {
        return Err(anyhow!("Invalid DROP MATERIALIZED VIEW syntax"));
    }
    i += 3;

    let mut if_exists = false;
    if tokens.get(i).is_some_and(|t| is_unquoted_keyword(t, "IF"))
        && tokens
            .get(i + 1)
            .is_some_and(|t| is_unquoted_keyword(t, "EXISTS"))
    {
        if_exists = true;
        i += 2;
    }

    if !matches!(tokens.get(i), Some(Token::Word(_))) {
        return Err(anyhow!("Missing view name"));
    }

    let mut names = Vec::new();
    let (name, consumed) = parse_object_name(tokens.get(i..).unwrap_or_default())?;
    names.push(name);
    i += consumed;

    while matches!(tokens.get(i), Some(Token::Comma)) {
        i += 1;
        if !matches!(tokens.get(i), Some(Token::Word(_))) {
            return Err(anyhow!("Missing view name"));
        }
        let (name, consumed) = parse_object_name(tokens.get(i..).unwrap_or_default())?;
        names.push(name);
        i += consumed;
    }

    let mut cascade = false;
    if tokens
        .get(i)
        .is_some_and(|t| is_unquoted_keyword(t, "CASCADE"))
    {
        cascade = true;
    }

    Ok((names, if_exists, cascade))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_refresh_materialized_view_preserves_quoted_ident_case() {
        let name =
            parse_refresh_materialized_view_name(r#"REFRESH MATERIALIZED VIEW "MyMV";"#).unwrap();
        assert_eq!(name.0.len(), 1);
        assert_eq!(name.0[0].value, "MyMV");
        assert_eq!(name.0[0].quote_style, Some('"'));
    }

    #[test]
    fn parse_drop_materialized_view_preserves_quoted_ident_case() {
        let (names, if_exists, cascade) =
            parse_drop_materialized_view(r#"DROP MATERIALIZED VIEW IF EXISTS "MyMV";"#).unwrap();
        assert!(if_exists);
        assert!(!cascade);
        assert_eq!(names.len(), 1);
        assert_eq!(names[0].0.len(), 1);
        assert_eq!(names[0].0[0].value, "MyMV");
        assert_eq!(names[0].0[0].quote_style, Some('"'));
    }

    #[test]
    fn parse_refresh_materialized_view_supports_concurrently() {
        let name = parse_refresh_materialized_view_name(
            r#"REFRESH MATERIALIZED VIEW CONCURRENTLY public."MyMV";"#,
        )
        .unwrap();
        assert_eq!(name.0.len(), 2);
        assert_eq!(name.0[0].value, "public");
        assert_eq!(name.0[0].quote_style, None);
        assert_eq!(name.0[1].value, "MyMV");
        assert_eq!(name.0[1].quote_style, Some('"'));
    }

    #[test]
    fn parse_drop_materialized_view_supports_multiple_names() {
        let (names, if_exists, _cascade) = parse_drop_materialized_view(
            r#"DROP MATERIALIZED VIEW IF EXISTS public."MyMV", "Other";"#,
        )
        .unwrap();
        assert!(if_exists);
        assert_eq!(names.len(), 2);
        assert_eq!(names[0].0.len(), 2);
        assert_eq!(names[0].0[1].value, "MyMV");
        assert_eq!(names[0].0[1].quote_style, Some('"'));
        assert_eq!(names[1].0.len(), 1);
        assert_eq!(names[1].0[0].value, "Other");
        assert_eq!(names[1].0[0].quote_style, Some('"'));
    }

    #[test]
    fn parse_drop_materialized_view_cascade() {
        let (names, if_exists, cascade) =
            parse_drop_materialized_view(r#"DROP MATERIALIZED VIEW mv1 CASCADE;"#).unwrap();
        assert!(!if_exists);
        assert!(cascade);
        assert_eq!(names.len(), 1);
        assert_eq!(names[0].0[0].value, "mv1");
    }

    #[test]
    fn parse_call_arguments_handles_quoted_strings_with_commas() {
        let args = parse_call_arguments("'hello, world', 42").unwrap();
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "'hello, world'");
        assert_eq!(args[1], "42");
    }

    #[test]
    fn parse_call_arguments_handles_nested_parentheses() {
        let args = parse_call_arguments("func(1, 2), 'test'").unwrap();
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "func(1, 2)");
        assert_eq!(args[1], "'test'");
    }

    #[test]
    fn parse_call_arguments_handles_empty_string() {
        let args = parse_call_arguments("").unwrap();
        assert_eq!(args.len(), 0);
    }

    #[test]
    fn parse_call_arguments_handles_array_with_commas() {
        // Regression test: commas inside ARRAY[...] should not split arguments
        let args = parse_call_arguments("ARRAY[1,2], 3").unwrap();
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "ARRAY[1,2]");
        assert_eq!(args[1], "3");
    }

    #[test]
    fn parse_call_arguments_handles_braces_with_commas() {
        // Test curly braces nesting
        let args = parse_call_arguments("{1,2,3}, 'test'").unwrap();
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "{1,2,3}");
        assert_eq!(args[1], "'test'");
    }

    #[test]
    fn parse_call_arguments_handles_mixed_nesting() {
        // Test mixed parentheses, brackets, and braces
        let args = parse_call_arguments("func(ARRAY[1,2], {3,4}), 5").unwrap();
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "func(ARRAY[1,2], {3,4})");
        assert_eq!(args[1], "5");
    }

    #[test]
    fn substitute_parameters_only_replaces_unquoted_identifiers() {
        let mut param_map = HashMap::new();
        param_map.insert("id".to_string(), ("123".to_string(), "int".to_string()));

        let result = substitute_parameters_in_statement(
            "SELECT id, user_id, 'id' FROM users WHERE id = id",
            &param_map,
        )
        .unwrap();

        // Should replace unquoted 'id' but not 'user_id' or string literal 'id'
        assert!(result.contains("SELECT 123"));
        assert!(result.contains("user_id"));
        assert!(result.contains("'id'"));
        assert!(result.contains("WHERE 123 = 123"));
    }

    #[test]
    fn substitute_parameters_preserves_quoted_identifiers() {
        let mut param_map = HashMap::new();
        param_map.insert(
            "name".to_string(),
            ("'test'".to_string(), "text".to_string()),
        );

        let result =
            substitute_parameters_in_statement(r#"SELECT "name", name FROM users"#, &param_map)
                .unwrap();

        // Should not replace quoted identifier "name" but should replace unquoted name
        assert!(result.contains(r#""name""#));
        assert!(result.contains("'test'"));
    }

    #[test]
    fn parse_call_arguments_handles_escaped_string_literals_with_quotes() {
        // Test E'...' with embedded quote: E'it\'s fine'
        let args = parse_call_arguments(r"E'it\'s fine'").unwrap();
        assert_eq!(args.len(), 1);
        assert_eq!(args[0], r"E'it''s fine'");
    }

    #[test]
    fn parse_call_arguments_handles_escaped_string_literals_with_backslashes() {
        // Test E'...' with backslash: E'\\n' should remain as E'\\n' not E'\n'
        let args = parse_call_arguments(r"E'\\n'").unwrap();
        assert_eq!(args.len(), 1);
        assert_eq!(args[0], r"E'\\n'");
    }

    #[test]
    fn parse_call_arguments_handles_escaped_string_literals_mixed() {
        // Test E'...' with both backslashes and quotes
        let args = parse_call_arguments(r"E'path\\to\'file'").unwrap();
        assert_eq!(args.len(), 1);
        assert_eq!(args[0], r"E'path\\to''file'");
    }

    #[test]
    fn parse_call_arguments_handles_multiple_escaped_string_literals() {
        let args = parse_call_arguments(r"E'it\'s', E'\\test', 42").unwrap();
        assert_eq!(args.len(), 3);
        assert_eq!(args[0], r"E'it''s'");
        assert_eq!(args[1], r"E'\\test'");
        assert_eq!(args[2], "42");
    }

    #[test]
    fn substitute_parameters_handles_escaped_string_literals_with_quotes() {
        let param_map = HashMap::new();
        let result =
            substitute_parameters_in_statement(r"SELECT E'it\'s fine' FROM t", &param_map).unwrap();
        assert!(result.contains(r"E'it''s fine'"));
    }

    #[test]
    fn substitute_parameters_handles_escaped_string_literals_with_backslashes() {
        let param_map = HashMap::new();
        let result =
            substitute_parameters_in_statement(r"SELECT E'\\n' FROM t", &param_map).unwrap();
        assert!(result.contains(r"E'\\n'"));
    }

    #[test]
    fn substitute_parameters_handles_escaped_string_literals_mixed() {
        let param_map = HashMap::new();
        let result =
            substitute_parameters_in_statement(r"SELECT E'path\\to\'file' FROM t", &param_map)
                .unwrap();
        assert!(result.contains(r"E'path\\to''file'"));
    }
}

impl Executor {
    pub(crate) async fn execute_create_materialized_view(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        name: &ObjectName,
        query: &Query,
        or_replace: bool,
        current_role: Option<&str>,
    ) -> Result<ExecuteResult> {
        let result = self
            .execute_query_with_ctes(
                txn,
                db_id,
                sequence_values,
                search_path,
                query,
                &HashMap::new(),
                current_role,
            )
            .await?;
        let (columns, rows) = match result {
            ExecuteResult::Select {
                columns,
                column_types: _,
                rows,
                timezone: _,
            } => (columns, rows),
            _ => return Err(anyhow!("Materialized view must be a SELECT query")),
        };

        let resolved = names::resolve_ddl_object_name(name, search_path)?;
        if !self
            .store()
            .schema_exists(txn, db_id, &resolved.schema)
            .await?
        {
            return Err(anyhow!("schema '{}' does not exist", resolved.schema));
        }
        let view_name = resolved.full;

        let table_id = self.store().next_table_id(txn, db_id).await?;
        let mut col_defs: Vec<ColumnDef> = vec![ColumnDef {
            name: "_mv_rowid".to_string(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: true,
            default_expr: None,
            is_serial: true,
            unique: true,
        }];
        col_defs.extend(columns.iter().enumerate().map(|(i, col_name)| {
            let data_type = if rows.is_empty() {
                DataType::Text
            } else {
                infer_data_type(&rows[0].values[i])
            };
            ColumnDef {
                name: col_name.clone(),
                data_type,
                nullable: true,
                primary_key: false,
                default_expr: None,
                is_serial: false,
                unique: false,
            }
        }));

        let rows_with_rowid: Vec<Row> = rows
            .into_iter()
            .enumerate()
            .map(|(i, mut row)| {
                let mut values = vec![Value::Int64((i + 1) as i64)];
                values.append(&mut row.values);
                Row::new(values)
            })
            .collect();

        let schema = TableSchema {
            table_id,
            name: view_name.clone(),
            columns: col_defs,
            version: 1,
            pk_constraint_name: Some(format!(
                "{}_pkey",
                view_name.rsplit('.').next().unwrap_or(&view_name)
            )),
            pk_indices: vec![0],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: "postgres".to_string(),
            from_alias: None,
        };

        ddl::execute_create_materialized_view(
            &self.store(),
            txn,
            db_id,
            search_path,
            name,
            query,
            or_replace,
            schema,
            rows_with_rowid,
        )
        .await
    }

    pub(crate) async fn execute_refresh_materialized_view_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let view_obj = parse_refresh_materialized_view_name(sql)?;
        let view_name_for_error = view_obj.to_string();

        // Detect CONCURRENTLY keyword
        let concurrently = sql.to_uppercase().contains("CONCURRENTLY");

        if concurrently {
            // Enqueue as BgDdl task and return immediately
            let is_autocommit = !session.is_in_transaction();
            if is_autocommit {
                session.begin().await?;
            }

            let result = async {
                let db_id = session.current_database_id();
                let (txn, _, search_path) = session
                    .get_mut_txn_sequence_values_and_search_path()
                    .expect("Transaction must be active");

                // Resolve view name to validate it exists
                let resolved = names::resolve_existing_materialized_view_name(
                    self.store().as_ref(),
                    txn,
                    db_id,
                    &view_obj,
                    search_path,
                )
                .await?
                .ok_or_else(|| anyhow!("Materialized view '{}' does not exist", view_name_for_error))?;
                let view_full_name = resolved.full;

                // Enqueue BgDdl task
                if let Some(system_store) = crate::worker::get_system_store() {
                    let keyspace = self.tenant_keyspace().to_string();
                    let username = session.current_user().map(|u| u.to_string()).unwrap_or_default();
                    
                    // Create command: REFRESH MATERIALIZED VIEW view_name (without CONCURRENTLY)
                    let command = format!("REFRESH MATERIALIZED VIEW {}", view_full_name);
                    
                    let entry = crate::worker::types::TaskQueueEntry::new(
                        keyspace.clone(),
                        db_id,
                        0i64, // task_id not used for REFRESH MV
                        crate::worker::types::TaskType::BgDdl,
                        command,
                        username,
                        128, // default priority
                    );
                    
                    let now_ms = chrono::Utc::now().timestamp_millis();
                    let mut sys_txn = system_store.begin().await?;
                    system_store
                        .put_worker_queue_entry(&mut sys_txn, &entry, now_ms)
                        .await?;
                    system_store
                        .update_registry_task_types(&mut sys_txn, &keyspace, db_id, crate::worker::types::TASK_TYPE_BG_DDL, 0)
                        .await?;
                    sys_txn.commit().await?;
                }

                Ok(ExecuteResult::RefreshMaterializedView {
                    view_name: view_full_name,
                })
            }
            .await;

            if is_autocommit {
                if result.is_ok() {
                    session.commit().await?;
                } else {
                    session.rollback().await?;
                }
            }

            return result;
        }

        // Non-concurrent path: execute synchronously
        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }

        // Extract role before mutable borrow of session for the transaction.
        let current_role = session.current_user().map(|u| u.to_string());

        let result = async {
            let db_id = session.current_database_id();
            let (txn, sequence_values, search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");

            let resolved = names::resolve_existing_materialized_view_name(
                self.store().as_ref(),
                txn,
                db_id,
                &view_obj,
                search_path,
            )
            .await?
            .ok_or_else(|| anyhow!("Materialized view '{}' does not exist", view_name_for_error))?;
            let view_full_name = resolved.full;

            let query_str = self
                .store()
                .get_materialized_view(txn, db_id, &view_full_name)
                .await?
                .ok_or_else(|| anyhow!("Materialized view '{}' does not exist", view_full_name))?
                .query;

            let ast = parse_sql(&query_str)?;
            let query = match ast.into_iter().next() {
                Some(Statement::Query(q)) => q,
                _ => return Err(anyhow!("Invalid materialized view query")),
            };

            let result = self
                .execute_query_with_ctes(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    &query,
                    &HashMap::new(),
                    current_role.as_deref(),
                )
                .await?;
            let rows = match result {
                ExecuteResult::Select { rows, .. } => rows,
                _ => return Err(anyhow!("Materialized view must be a SELECT query")),
            };

            let rows_with_rowid: Vec<Row> = rows
                .into_iter()
                .enumerate()
                .map(|(i, mut row)| {
                    let mut values = vec![Value::Int64((i + 1) as i64)];
                    values.append(&mut row.values);
                    Row::new(values)
                })
                .collect();

            ddl::execute_refresh_materialized_view(
                &self.store(),
                txn,
                db_id,
                &view_full_name,
                rows_with_rowid,
            )
            .await
        }
        .await;

        if is_autocommit {
            if result.is_ok() {
                session.commit().await?;
            } else {
                session.rollback().await?;
            }
        }

        result
    }

    pub(crate) async fn execute_drop_materialized_view_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let (names, if_exists, cascade) = parse_drop_materialized_view(sql)?;

        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }

        let result = async {
            let db_id = session.current_database_id();
            let (txn, _sequence_values, search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");
            ddl::execute_drop_materialized_view(
                &self.store(),
                txn,
                db_id,
                search_path,
                &names,
                if_exists,
                cascade,
            )
            .await
        }
        .await;

        if is_autocommit {
            if result.is_ok() {
                session.commit().await?;
            } else {
                session.rollback().await?;
            }
        }

        result
    }

    pub(crate) async fn execute_create_procedure(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        search_path: &[String],
        name: &ObjectName,
        params: Option<&[sqlparser::ast::ProcedureParam]>,
        body: &[Statement],
    ) -> Result<ExecuteResult> {
        let resolved = names::resolve_ddl_object_name(name, search_path)?;
        if !self
            .store()
            .schema_exists(txn, db_id, &resolved.schema)
            .await?
        {
            return Err(anyhow!("schema '{}' does not exist", resolved.schema));
        }
        let proc_name = resolved.full;

        let param_defs: Vec<String> = params
            .map(|p| {
                p.iter()
                    .map(|param| format!("{} {}", param.name.value, param.data_type))
                    .collect()
            })
            .unwrap_or_default();

        let body_stmts: Vec<String> = body.iter().map(|s| s.to_string()).collect();

        let definition = format!(
            "PARAMS:{}\nBODY:{}",
            param_defs.join(","),
            body_stmts.join(";")
        );

        self.store()
            .create_procedure(txn, db_id, &proc_name, &definition)
            .await?;

        Ok(ExecuteResult::CreateProcedure { proc_name })
    }

    pub(crate) async fn execute_call_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let sql_trimmed = sql.trim();
        let rest = sql_trimmed
            .strip_prefix("CALL ")
            .or_else(|| sql_trimmed.strip_prefix("call "))
            .ok_or_else(|| anyhow!("Invalid CALL syntax"))?
            .trim();

        let paren_pos = rest.find('(').unwrap_or(rest.len());
        let proc_name = rest[..paren_pos].trim().to_lowercase();

        let args_str = if let Some(start) = rest.find('(') {
            let end = rest.rfind(')').unwrap_or(rest.len());
            rest[start + 1..end].trim()
        } else {
            ""
        };

        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }

        let current_role = session.current_user().map(|u| u.to_string());
        let result = async {
            let db_id = session.current_database_id();
            let (txn, sequence_values, search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");

            let proc_obj = object_name_from_token(&proc_name)?;
            let resolved = names::resolve_existing_procedure_name(
                self.store().as_ref(),
                txn,
                db_id,
                &proc_obj,
                search_path,
            )
            .await?
            .ok_or_else(|| anyhow!("Procedure '{}' does not exist", proc_name))?;
            let proc_full_name = resolved.full;

            let definition: String = self
                .store()
                .get_procedure(txn, db_id, &proc_full_name)
                .await?
                .ok_or_else(|| anyhow!("Procedure '{}' does not exist", proc_full_name))?;

            let parts: Vec<&str> = definition.splitn(2, "\nBODY:").collect();
            if parts.len() != 2 {
                return Err(anyhow!("Invalid procedure definition"));
            }

            let param_str = parts[0].strip_prefix("PARAMS:").unwrap_or("");
            let body_str = parts[1];

            let param_defs: Vec<(&str, &str)> = if param_str.is_empty() {
                vec![]
            } else {
                param_str
                    .split(',')
                    .filter_map(|p| {
                        let parts: Vec<&str> = p.trim().splitn(2, ' ').collect();
                        if parts.len() == 2 {
                            Some((parts[0], parts[1]))
                        } else {
                            None
                        }
                    })
                    .collect()
            };

            let call_args = parse_call_arguments(args_str)?;

            if call_args.len() != param_defs.len() {
                return Err(anyhow!(
                    "Procedure '{}' expects {} arguments, got {}",
                    proc_full_name,
                    param_defs.len(),
                    call_args.len()
                ));
            }

            let mut param_map: HashMap<String, (String, String)> = HashMap::new();
            for (i, (name, data_type)) in param_defs.iter().enumerate() {
                param_map.insert(
                    name.to_string(),
                    (call_args[i].clone(), data_type.to_string()),
                );
            }

            let body_statements: Vec<&str> = body_str
                .split(';')
                .filter(|s| !s.trim().is_empty())
                .collect();

            for stmt_str in body_statements {
                let expanded_stmt = substitute_parameters_in_statement(stmt_str, &param_map)?;

                let stmts = parse_sql(&expanded_stmt)?;
                for stmt in stmts {
                    self.execute_statement_on_txn(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        &stmt,
                        current_role.as_deref(),
                    )
                    .await?;
                }
            }

            Ok(ExecuteResult::Call)
        }
        .await;

        if is_autocommit {
            if result.is_ok() {
                session.commit().await?;
            } else {
                session.rollback().await?;
            }
        }

        result
    }

    pub(crate) async fn execute_create_procedure_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let sql_trimmed = sql.trim();
        let sql_upper = sql_trimmed.to_uppercase();
        let is_or_replace = sql_upper.starts_with("CREATE OR REPLACE PROCEDURE");
        let rest = sql_trimmed
            .strip_prefix("CREATE PROCEDURE")
            .or_else(|| sql_trimmed.strip_prefix("CREATE OR REPLACE PROCEDURE"))
            .or_else(|| sql_trimmed.strip_prefix("create procedure"))
            .or_else(|| sql_trimmed.strip_prefix("create or replace procedure"))
            .ok_or_else(|| anyhow!("Invalid CREATE PROCEDURE syntax"))?
            .trim();

        let (name_and_params, body) = rest
            .split_once("AS BEGIN")
            .or_else(|| rest.split_once("as begin"))
            .or_else(|| rest.split_once("AS\nBEGIN"))
            .or_else(|| rest.split_once("as\nbegin"))
            .ok_or_else(|| anyhow!("CREATE PROCEDURE requires AS BEGIN ... END syntax"))?;

        let body_trimmed = body.trim_end();
        let body_trimmed = body_trimmed
            .strip_suffix(';')
            .unwrap_or(body_trimmed)
            .trim_end();
        if body_trimmed.len() < 3
            || !body_trimmed.as_bytes()[body_trimmed.len() - 3..].eq_ignore_ascii_case(b"END")
        {
            return Err(anyhow!("CREATE PROCEDURE requires AS BEGIN ... END syntax"));
        }
        let body = body_trimmed[..body_trimmed.len() - 3].trim();

        let name_and_params = name_and_params.trim();
        let (proc_name, params_str) = if let Some(paren_pos) = name_and_params.find('(') {
            let name = name_and_params[..paren_pos].trim().to_lowercase();
            let params = name_and_params[paren_pos..]
                .trim_start_matches('(')
                .trim_end_matches(')')
                .trim();
            (name, params)
        } else {
            (name_and_params.to_lowercase(), "")
        };

        let definition = format!("PARAMS:{}\nBODY:{}", params_str, body);

        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }

        let result = async {
            let db_id = session.current_database_id();
            let (txn, _sequence_values, search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");
            let proc_obj = object_name_from_token(&proc_name)?;
            let resolved = names::resolve_ddl_object_name(&proc_obj, search_path)?;
            if !self
                .store()
                .schema_exists(txn, db_id, &resolved.schema)
                .await?
            {
                return Err(anyhow!("schema '{}' does not exist", resolved.schema));
            }
            let proc_full_name = resolved.full;
            if is_or_replace {
                self.store()
                    .replace_procedure(txn, db_id, &proc_full_name, &definition)
                    .await?;
            } else {
                self.store()
                    .create_procedure(txn, db_id, &proc_full_name, &definition)
                    .await?;
            }
            Ok(ExecuteResult::CreateProcedure {
                proc_name: proc_full_name,
            })
        }
        .await;

        if is_autocommit {
            if result.is_ok() {
                session.commit().await?;
            } else {
                session.rollback().await?;
            }
        }

        result
    }

    pub(crate) async fn execute_drop_procedure_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let sql_upper = sql.trim().to_uppercase();
        let rest = sql_upper
            .strip_prefix("DROP PROCEDURE")
            .ok_or_else(|| anyhow!("Invalid DROP PROCEDURE syntax"))?
            .trim();

        let if_exists = rest.starts_with("IF EXISTS");
        let name_part = if if_exists {
            rest.strip_prefix("IF EXISTS").unwrap().trim()
        } else {
            rest
        };

        let proc_name = name_part
            .split(|c: char| c.is_whitespace() || c == '(' || c == ';')
            .next()
            .ok_or_else(|| anyhow!("Missing procedure name"))?
            .to_lowercase();

        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }

        let result = async {
            let db_id = session.current_database_id();
            let (txn, _sequence_values, search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");
            let proc_obj = object_name_from_token(&proc_name)?;
            let resolved = names::resolve_existing_procedure_name(
                self.store().as_ref(),
                txn,
                db_id,
                &proc_obj,
                search_path,
            )
            .await?;

            let proc_full_name = match resolved {
                Some(resolved) => resolved.full,
                None => names::resolve_ddl_object_name(&proc_obj, search_path)?.full,
            };

            let dropped = self
                .store()
                .drop_procedure(txn, db_id, &proc_full_name)
                .await?;
            if !dropped && !if_exists {
                return Err(anyhow!("Procedure '{}' does not exist", proc_full_name));
            }
            Ok(ExecuteResult::DropProcedure {
                proc_name: proc_full_name,
            })
        }
        .await;

        if is_autocommit {
            if result.is_ok() {
                session.commit().await?;
            } else {
                session.rollback().await?;
            }
        }

        result
    }
}
