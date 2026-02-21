use super::super::expr::bridge::eval_const_ast_expr;
use super::super::names;
use super::super::names::normalize_ident;
use super::super::Session;
use super::core::Executor;
use super::triggers::strip_leading_sql_comments;
use crate::extensions::{descriptor, InstalledExtension};
use crate::sql::error::SqlError;
use crate::sql::operators::{BoxedOperator, TableFunctionScanOperator};
use anyhow::{anyhow, Result};
use sqlparser::ast::{Expr, FunctionArg, FunctionArgExpr, ObjectName, TableAlias};
use tikv_client::Transaction;
use tracing::info;

use crate::extensions::EXTENSIONS_SCHEMA;
use crate::types::{Row, TableSchema, Value};

use crate::extensions::fs::{self, Fs9Mode};
use crate::extensions::http::{self, HttpTableFunctionCall};

/// Result of executing an extension table function.
/// Streaming mode returns an operator that yields rows lazily.
/// Batch mode returns all rows materialized in a Vec.
pub(crate) enum ExtensionTableFunctionResult {
    Batch(TableSchema, Vec<Row>),
    Streaming(TableSchema, BoxedOperator),
}

fn apply_table_function_alias(schema: &mut TableSchema, alias: Option<&TableAlias>) -> Result<()> {
    if let Some(alias) = alias {
        if !alias.columns.is_empty() {
            if alias.columns.len() != schema.columns.len() {
                return Err(anyhow!(
                    "Table function alias column count mismatch: expected {}, got {}",
                    schema.columns.len(),
                    alias.columns.len()
                ));
            }
            for (i, ident) in alias.columns.iter().enumerate() {
                schema.columns[i].name = normalize_ident(ident);
            }
        }
    }
    Ok(())
}

fn starts_with_ignore_ascii_case(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix)
}

fn parse_extension_name_token(token: &str) -> Result<String> {
    let token = token.trim().trim_end_matches(';');
    if token.is_empty() {
        return Err(anyhow!("Missing extension name"));
    }
    let name = token
        .split('.')
        .last()
        .ok_or_else(|| anyhow!("Missing extension name"))?
        .trim_matches('"')
        .to_lowercase();
    if name.is_empty() {
        return Err(anyhow!("Missing extension name"));
    }
    Ok(name)
}

fn parse_create_extension_sql(sql: &str) -> Result<(bool, String)> {
    let sql = strip_leading_sql_comments(sql).trim();
    let sql = sql.trim_end_matches(';').trim_end();

    let rest = if starts_with_ignore_ascii_case(sql, "create extension") {
        sql["create extension".len()..].trim_start()
    } else {
        return Err(anyhow!("Invalid CREATE EXTENSION syntax"));
    };

    let if_not_exists = starts_with_ignore_ascii_case(rest, "if not exists");
    let rest = if if_not_exists {
        rest["if not exists".len()..].trim_start()
    } else {
        rest
    };

    let mut iter = rest.split_whitespace();
    let ext_tok = iter
        .next()
        .ok_or_else(|| anyhow!("Missing extension name"))?;
    let ext_name = parse_extension_name_token(ext_tok)?;
    Ok((if_not_exists, ext_name))
}

fn parse_drop_extension_sql(sql: &str) -> Result<(bool, String)> {
    let sql = strip_leading_sql_comments(sql).trim();
    let sql = sql.trim_end_matches(';').trim_end();

    let rest = if starts_with_ignore_ascii_case(sql, "drop extension") {
        sql["drop extension".len()..].trim_start()
    } else {
        return Err(anyhow!("Invalid DROP EXTENSION syntax"));
    };

    let if_exists = starts_with_ignore_ascii_case(rest, "if exists");
    let rest = if if_exists {
        rest["if exists".len()..].trim_start()
    } else {
        rest
    };

    let mut iter = rest.split_whitespace();
    let ext_tok = iter
        .next()
        .ok_or_else(|| anyhow!("Missing extension name"))?;
    let ext_name = parse_extension_name_token(ext_tok)?;
    Ok((if_exists, ext_name))
}

impl Executor {
    pub(crate) async fn try_execute_extension_table_function(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        search_path: &[String],
        name: &ObjectName,
        args: &[FunctionArg],
        alias: Option<&TableAlias>,
    ) -> Result<Option<ExtensionTableFunctionResult>> {
        let (schema_opt, func_name) = names::split_object_name(name)?;

        let schema = match schema_opt {
            Some(schema) => schema,
            None => {
                if search_path
                    .iter()
                    .any(|s| s.eq_ignore_ascii_case(EXTENSIONS_SCHEMA))
                {
                    EXTENSIONS_SCHEMA.to_string()
                } else {
                    return Ok(None);
                }
            }
        };

        if !schema.eq_ignore_ascii_case(EXTENSIONS_SCHEMA) {
            return Ok(None);
        }

        fn extract_expr_arg(arg: &FunctionArg) -> Result<&Expr> {
            match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Ok(e),
                _ => Err(anyhow!("http: requires expression arguments")),
            }
        }

        fn expect_text(v: Value, what: &str) -> Result<String> {
            match v {
                Value::Text(s) => Ok(s),
                Value::Null => Err(anyhow!("http: {} must not be NULL", what)),
                other => Err(anyhow!(
                    "http: {} must be TEXT, got {}",
                    what,
                    other.data_type().unwrap_or(crate::types::DataType::Text)
                )),
            }
        }

        fn extract_optional_headers(args: &[FunctionArg], idx: usize) -> Result<Option<String>> {
            if idx >= args.len() {
                return Ok(None);
            }
            let val = eval_const_ast_expr(extract_expr_arg(&args[idx])?)?;
            match val {
                Value::Null => Ok(None),
                Value::Text(s) | Value::Jsonb(s) => Ok(Some(s)),
                other => Err(anyhow!(
                    "http: headers must be JSONB, got {}",
                    other.data_type().unwrap_or(crate::types::DataType::Text)
                )),
            }
        }

        let func_upper = func_name.to_ascii_uppercase();
        let http_call = match func_upper.as_str() {
            "HTTP_GET" => {
                if args.is_empty() || args.len() > 2 {
                    return Err(anyhow!(
                        "http_get(url text [, headers jsonb]) requires 1-2 arguments"
                    ));
                }
                let url = expect_text(eval_const_ast_expr(extract_expr_arg(&args[0])?)?, "url")?;
                let headers = extract_optional_headers(args, 1)?;
                Some(HttpTableFunctionCall::Get { url, headers })
            }
            "HTTP_HEAD" => {
                if args.is_empty() || args.len() > 2 {
                    return Err(anyhow!(
                        "http_head(url text [, headers jsonb]) requires 1-2 arguments"
                    ));
                }
                let url = expect_text(eval_const_ast_expr(extract_expr_arg(&args[0])?)?, "url")?;
                let headers = extract_optional_headers(args, 1)?;
                Some(HttpTableFunctionCall::Head { url, headers })
            }
            "HTTP_DELETE" => {
                if args.is_empty() || args.len() > 2 {
                    return Err(anyhow!(
                        "http_delete(url text [, headers jsonb]) requires 1-2 arguments"
                    ));
                }
                let url = expect_text(eval_const_ast_expr(extract_expr_arg(&args[0])?)?, "url")?;
                let headers = extract_optional_headers(args, 1)?;
                Some(HttpTableFunctionCall::Delete { url, headers })
            }
            "HTTP_POST" => {
                if args.len() < 3 || args.len() > 4 {
                    return Err(anyhow!(
                        "http_post(url text, body text, content_type text [, headers jsonb]) requires 3-4 arguments"
                    ));
                }
                let url = expect_text(eval_const_ast_expr(extract_expr_arg(&args[0])?)?, "url")?;
                let body = expect_text(eval_const_ast_expr(extract_expr_arg(&args[1])?)?, "body")?;
                let content_type = expect_text(
                    eval_const_ast_expr(extract_expr_arg(&args[2])?)?,
                    "content_type",
                )?;
                let headers = extract_optional_headers(args, 3)?;
                Some(HttpTableFunctionCall::Post {
                    url,
                    body,
                    content_type,
                    headers,
                })
            }
            "HTTP_PUT" => {
                if args.len() < 3 || args.len() > 4 {
                    return Err(anyhow!(
                        "http_put(url text, body text, content_type text [, headers jsonb]) requires 3-4 arguments"
                    ));
                }
                let url = expect_text(eval_const_ast_expr(extract_expr_arg(&args[0])?)?, "url")?;
                let body = expect_text(eval_const_ast_expr(extract_expr_arg(&args[1])?)?, "body")?;
                let content_type = expect_text(
                    eval_const_ast_expr(extract_expr_arg(&args[2])?)?,
                    "content_type",
                )?;
                let headers = extract_optional_headers(args, 3)?;
                Some(HttpTableFunctionCall::Put {
                    url,
                    body,
                    content_type,
                    headers,
                })
            }
            "HTTP_PATCH" => {
                if args.len() < 3 || args.len() > 4 {
                    return Err(anyhow!(
                        "http_patch(url text, body text, content_type text [, headers jsonb]) requires 3-4 arguments"
                    ));
                }
                let url = expect_text(eval_const_ast_expr(extract_expr_arg(&args[0])?)?, "url")?;
                let body = expect_text(eval_const_ast_expr(extract_expr_arg(&args[1])?)?, "body")?;
                let content_type = expect_text(
                    eval_const_ast_expr(extract_expr_arg(&args[2])?)?,
                    "content_type",
                )?;
                let headers = extract_optional_headers(args, 3)?;
                Some(HttpTableFunctionCall::Universal {
                    method: "PATCH".to_string(),
                    url,
                    headers,
                    content_type: Some(content_type),
                    body: Some(body),
                })
            }
            "HTTP" => {
                // http(method, uri [, headers jsonb [, content_type text [, content text]]])
                if args.len() < 2 || args.len() > 5 {
                    return Err(anyhow!(
                        "http(method text, uri text [, headers jsonb [, content_type text [, content text]]]) requires 2-5 arguments"
                    ));
                }
                let method =
                    expect_text(eval_const_ast_expr(extract_expr_arg(&args[0])?)?, "method")?;
                let url = expect_text(eval_const_ast_expr(extract_expr_arg(&args[1])?)?, "uri")?;
                let headers = extract_optional_headers(args, 2)?;
                let content_type = if args.len() > 3 {
                    match eval_const_ast_expr(extract_expr_arg(&args[3])?)? {
                        Value::Null => None,
                        Value::Text(s) => Some(s),
                        other => {
                            return Err(anyhow!(
                                "http: content_type must be TEXT, got {}",
                                other.data_type().unwrap_or(crate::types::DataType::Text)
                            ))
                        }
                    }
                } else {
                    None
                };
                let body = if args.len() > 4 {
                    match eval_const_ast_expr(extract_expr_arg(&args[4])?)? {
                        Value::Null => None,
                        Value::Text(s) => Some(s),
                        other => {
                            return Err(anyhow!(
                                "http: content must be TEXT, got {}",
                                other.data_type().unwrap_or(crate::types::DataType::Text)
                            ))
                        }
                    }
                } else {
                    None
                };
                Some(HttpTableFunctionCall::Universal {
                    method,
                    url,
                    headers,
                    content_type,
                    body,
                })
            }
            _ => None,
        };

        if let Some(call) = http_call {
            let installed = self.store().get_extension(txn, db_id, "http").await?;
            let Some(installed) = installed else {
                return Err(anyhow!("extension \"http\" is not installed"));
            };
            if !installed.enabled {
                return Err(anyhow!("extension \"http\" is disabled"));
            }

            let (mut schema, rows) =
                http::execute_table_function(self.tenant_keyspace(), call).await?;
            apply_table_function_alias(&mut schema, alias)?;
            return Ok(Some(ExtensionTableFunctionResult::Batch(schema, rows)));
        }

        if func_upper == "FS9" {
            if args.is_empty() {
                return Err(anyhow!("fs9(path text) requires at least 1 argument"));
            }

            // Extract first positional arg as path
            let path_expr = match &args[0] {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => e,
                FunctionArg::Named { .. } => {
                    return Err(anyhow!(
                        "fs9: first argument must be an unnamed path string"
                    ));
                }
                _ => return Err(anyhow!("fs9: first argument must be a path string")),
            };
            let path = match eval_const_ast_expr(path_expr)? {
                Value::Text(s) => s,
                Value::Null => return Err(anyhow!("fs9: path must not be NULL")),
                _ => return Err(anyhow!("fs9: path must be TEXT")),
            };

            // Parse named parameters from remaining args
            let mut format: Option<String> = None;
            let mut delimiter: Option<char> = None;
            let mut header: Option<bool> = None;
            let mut recursive: Option<bool> = None;
            let mut exclude: Option<String> = None;

            for arg in &args[1..] {
                match arg {
                    FunctionArg::Named {
                        name,
                        arg: FunctionArgExpr::Expr(e),
                        ..
                    } => {
                        let param_name = name.value.to_ascii_lowercase();
                        let val = eval_const_ast_expr(e)?;
                        match param_name.as_str() {
                            "format" => {
                                format = Some(match val {
                                    Value::Text(s) => s,
                                    _ => return Err(anyhow!("fs9: 'format' must be TEXT")),
                                });
                            }
                            "delimiter" => {
                                let s = match val {
                                    Value::Text(s) => s,
                                    _ => return Err(anyhow!("fs9: 'delimiter' must be TEXT")),
                                };
                                if s.len() != 1 {
                                    return Err(anyhow!(
                                        "fs9: 'delimiter' must be a single character"
                                    ));
                                }
                                delimiter = Some(s.chars().next().unwrap());
                            }
                            "header" => {
                                header = Some(match val {
                                    Value::Boolean(b) => b,
                                    Value::Text(s) if s.eq_ignore_ascii_case("true") => true,
                                    Value::Text(s) if s.eq_ignore_ascii_case("false") => false,
                                    _ => return Err(anyhow!("fs9: 'header' must be BOOLEAN")),
                                });
                            }
                            "recursive" => {
                                recursive = Some(match val {
                                    Value::Boolean(b) => b,
                                    Value::Text(s) if s.eq_ignore_ascii_case("true") => true,
                                    Value::Text(s) if s.eq_ignore_ascii_case("false") => false,
                                    _ => return Err(anyhow!("fs9: 'recursive' must be BOOLEAN")),
                                });
                            }
                            "exclude" => {
                                exclude = Some(match val {
                                    Value::Text(s) => s,
                                    _ => return Err(anyhow!("fs9: 'exclude' must be TEXT")),
                                });
                            }
                            _ => return Err(anyhow!("fs9: unknown parameter '{}'", param_name)),
                        }
                    }
                    _ => {
                        // Additional unnamed args after the first are not supported
                        return Err(anyhow!("fs9: unexpected positional argument (use named parameters like format => 'csv')"));
                    }
                }
            }

            // Determine mode based on path
            let mode = if path.ends_with('/') {
                Fs9Mode::Directory {
                    path,
                    recursive: recursive.unwrap_or(false),
                    exclude,
                }
            } else if fs::glob::is_glob_pattern(&path) {
                Fs9Mode::Glob {
                    pattern: path,
                    format,
                    delimiter,
                    header,
                    exclude,
                }
            } else {
                Fs9Mode::File {
                    path,
                    format,
                    delimiter,
                    header,
                }
            };

            let installed = self.store().get_extension(txn, db_id, "fs9").await?;
            let Some(installed) = installed else {
                return Err(anyhow!("extension \"fs9\" is not installed"));
            };
            if !installed.enabled {
                return Err(anyhow!("extension \"fs9\" is disabled"));
            }

            if let Fs9Mode::File {
                path,
                format,
                delimiter,
                header,
            } = &mode
            {
                if let Some((mut schema, receiver)) = fs::start_file_stream(
                    self.tenant_keyspace(),
                    path,
                    format.as_deref(),
                    *delimiter,
                    *header,
                )
                .await?
                {
                    apply_table_function_alias(&mut schema, alias)?;
                    let operator: BoxedOperator = Box::new(
                        TableFunctionScanOperator::new_with_channel(schema.clone(), receiver),
                    );
                    return Ok(Some(ExtensionTableFunctionResult::Streaming(
                        schema, operator,
                    )));
                }
            }

            if let Fs9Mode::Glob {
                pattern,
                format,
                delimiter,
                header,
                exclude,
            } = &mode
            {
                if let Some((mut schema, receiver)) = fs::start_glob_stream(
                    self.tenant_keyspace(),
                    pattern,
                    format.as_deref(),
                    *delimiter,
                    *header,
                    exclude.as_deref(),
                )
                .await?
                {
                    apply_table_function_alias(&mut schema, alias)?;
                    let operator: BoxedOperator = Box::new(
                        TableFunctionScanOperator::new_with_channel(schema.clone(), receiver),
                    );
                    return Ok(Some(ExtensionTableFunctionResult::Streaming(
                        schema, operator,
                    )));
                }
            }

            let (mut schema, rows) =
                fs::execute_table_function(self.tenant_keyspace(), mode).await?;
            apply_table_function_alias(&mut schema, alias)?;
            return Ok(Some(ExtensionTableFunctionResult::Batch(schema, rows)));
        }

        if func_upper == "FS9_EVENTS" {
            let installed = self.store().get_extension(txn, db_id, "fs9").await?;
            let Some(installed) = installed else {
                return Err(anyhow!("extension \"fs9\" is not installed"));
            };
            if !installed.enabled {
                return Err(anyhow!("extension \"fs9\" is disabled"));
            }

            let mut limit: usize = 100;
            let mut offset: usize = 0;
            let mut path_filter: Option<String> = None;
            let mut type_filter: Option<String> = None;

            for (i, arg) in args.iter().enumerate() {
                match arg {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                        let val = eval_const_ast_expr(e)?;
                        match i {
                            0 => match val {
                                Value::Int64(n) => limit = n.max(0) as usize,
                                Value::Null => {}
                                _ => return Err(anyhow!("fs9_events: limit must be INT")),
                            },
                            1 => match val {
                                Value::Int64(n) => offset = n.max(0) as usize,
                                Value::Null => {}
                                _ => return Err(anyhow!("fs9_events: offset must be INT")),
                            },
                            2 => match val {
                                Value::Text(s) => path_filter = Some(s),
                                Value::Null => {}
                                _ => return Err(anyhow!("fs9_events: path must be TEXT")),
                            },
                            3 => match val {
                                Value::Text(s) => type_filter = Some(s),
                                Value::Null => {}
                                _ => return Err(anyhow!("fs9_events: type must be TEXT")),
                            },
                            _ => return Err(anyhow!("fs9_events: too many arguments (max 4)")),
                        }
                    }
                    _ => return Err(anyhow!("fs9_events: only positional arguments supported")),
                }
            }

            let (mut schema, rows) = fs::execute_fs9_events(
                self.tenant_keyspace(),
                limit,
                offset,
                path_filter.as_deref(),
                type_filter.as_deref(),
            )
            .await?;
            apply_table_function_alias(&mut schema, alias)?;
            return Ok(Some(ExtensionTableFunctionResult::Batch(schema, rows)));
        }

        Ok(None)
    }

    pub(crate) async fn execute_create_extension_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<super::super::ExecuteResult> {
        let (if_not_exists, ext_name) = parse_create_extension_sql(sql)?;

        if !session.is_superuser() {
            return Err(SqlError::PermissionDenied {
                object_type: "extension".into(),
                object_name: ext_name.clone(),
            }
            .into());
        }

        let desc = descriptor(&ext_name)
            .ok_or_else(|| anyhow!("Extension '{}' is not available", ext_name))?;
        let ext_default_schema = desc.default_schema.to_string();

        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }

        let result = async {
            let db_id = session.current_database_id();
            let (txn, _sequence_values, _search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");

            if !self
                .store()
                .schema_exists(txn, db_id, desc.default_schema)
                .await?
            {
                self.store()
                    .create_schema(txn, db_id, desc.default_schema, false)
                    .await?;
            }

            if self
                .store()
                .get_extension(txn, db_id, &ext_name)
                .await?
                .is_some()
            {
                if if_not_exists {
                    return Ok(super::super::ExecuteResult::CreateExtension {
                        ext_name: ext_name.clone(),
                    });
                }
                return Err(anyhow!("extension \"{}\" already exists", ext_name));
            }

            let ext = InstalledExtension::new(desc);
            self.store().put_extension(txn, db_id, &ext).await?;

            if ext_name == "pg_cron" {
                self.store().set_cron_enabled(txn, db_id).await?;
            }

            Ok(super::super::ExecuteResult::CreateExtension {
                ext_name: ext_name.clone(),
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

        if result.is_ok() {
            let sp = session.search_path();
            let mut new_sp = sp.to_vec();
            let mut modified = false;

            if !new_sp
                .iter()
                .any(|s| s.eq_ignore_ascii_case(EXTENSIONS_SCHEMA))
            {
                new_sp.push(EXTENSIONS_SCHEMA.to_string());
                modified = true;
            }

            if !ext_default_schema.eq_ignore_ascii_case(EXTENSIONS_SCHEMA)
                && !new_sp
                    .iter()
                    .any(|s| s.eq_ignore_ascii_case(&ext_default_schema))
            {
                new_sp.push(ext_default_schema);
                modified = true;
            }

            if modified {
                session.set_search_path(new_sp);
            }
        }

        result
    }

    pub(crate) async fn execute_drop_extension_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<super::super::ExecuteResult> {
        let (if_exists, ext_name) = parse_drop_extension_sql(sql)?;

        if !session.is_superuser() {
            return Err(SqlError::PermissionDenied {
                object_type: "extension".into(),
                object_name: ext_name.clone(),
            }
            .into());
        }

        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }

        let result = async {
            let db_id = session.current_database_id();
            let (txn, _sequence_values, _search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");

            if ext_name == "pg_cron" {
                self.store().remove_cron_enabled(txn, db_id).await?;
                self.store().delete_all_cron_data(txn, db_id).await?;
                info!(
                    "DROP EXTENSION pg_cron: all cron data deleted for db_id={}",
                    db_id
                );
            }

            let dropped = self.store().drop_extension(txn, db_id, &ext_name).await?;
            if !dropped && !if_exists {
                return Err(anyhow!("extension \"{}\" does not exist", ext_name));
            }

            if ext_name == "pg_cron" {
                let cron_tables: Vec<String> = self
                    .store()
                    .list_tables(txn, db_id)
                    .await?
                    .into_iter()
                    .filter(|t| t.starts_with("cron."))
                    .collect();
                if cron_tables.is_empty() {
                    let _ = self
                        .store()
                        .drop_schema_cascade(txn, db_id, "cron", true)
                        .await;
                }
            }

            Ok(super::super::ExecuteResult::DropExtension {
                ext_name: ext_name.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_create_extension_basic() {
        let (if_not_exists, name) = parse_create_extension_sql("CREATE EXTENSION http;").unwrap();
        assert!(!if_not_exists);
        assert_eq!(name, "http");
    }

    #[test]
    fn test_parse_create_extension_if_not_exists_case_insensitive() {
        let (if_not_exists, name) =
            parse_create_extension_sql("create extension IF NOT EXISTS \"http\"").unwrap();
        assert!(if_not_exists);
        assert_eq!(name, "http");
    }

    #[test]
    fn test_parse_drop_extension_if_exists_and_schema_prefix() {
        let (if_exists, name) =
            parse_drop_extension_sql("DROP EXTENSION IF EXISTS extensions.http").unwrap();
        assert!(if_exists);
        assert_eq!(name, "http");
    }
}
