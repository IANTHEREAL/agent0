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
use chrono::DateTime;
use sqlparser::ast::{Expr, FunctionArg, FunctionArgExpr, ObjectName, TableAlias};
use tikv_client::Transaction;
use tracing::info;

use crate::extensions::EXTENSIONS_SCHEMA;
use crate::model::{Row, TableSchema, Value};

use crate::extensions::fs::{self, Fs9Mode};
use crate::extensions::http::{self, HttpTableFunctionCall};

/// Result of executing an extension table function.
/// Streaming mode returns an operator that yields rows lazily.
/// Batch mode returns all rows materialized in a Vec.
#[allow(dead_code)] // framework: extension registry
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

fn parse_iso8601_to_epoch_ms(input: &str) -> Result<i64> {
    let dt = DateTime::parse_from_rfc3339(input).map_err(|e| {
        anyhow!(
            "embedding_usage: invalid resets_at timestamp '{}': {}",
            input,
            e
        )
    })?;
    Ok(dt.timestamp_millis())
}

fn embedding_usage_args_error(args_len: usize) -> anyhow::Error {
    let signature = if args_len == 0 {
        "extensions.embedding_usage()".to_string()
    } else {
        format!(
            "extensions.embedding_usage({})",
            vec!["unknown"; args_len].join(", ")
        )
    };
    SqlError::FunctionNotFound(signature).into()
}

fn embedding_usage_permission_error() -> anyhow::Error {
    SqlError::PermissionDenied {
        object_type: "function".into(),
        object_name: "embedding_usage".into(),
    }
    .into()
}

use super::core::starts_with_ignore_ascii_case;

fn parse_extension_name_token(token: &str) -> Result<String> {
    let token = token.trim().trim_end_matches(';');
    if token.is_empty() {
        return Err(anyhow!("Missing extension name"));
    }
    let name = token
        .rsplit('.')
        .next()
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

fn validate_extension_runtime_requirements(ext_name: &str, worker_available: bool) -> Result<()> {
    if ext_name.eq_ignore_ascii_case("pg_cron") && !worker_available {
        return Err(anyhow!(
            "CREATE EXTENSION pg_cron requires the worker subsystem \
             (DB9_WORKER_ENABLED=false). Cron jobs cannot execute without the worker engine."
        ));
    }
    Ok(())
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
                    other.data_type().unwrap_or(crate::model::DataType::Text)
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
                    other.data_type().unwrap_or(crate::model::DataType::Text)
                )),
            }
        }

        let func_upper = func_name.to_ascii_uppercase();
        if func_upper == "EMBEDDING_USAGE" {
            if !args.is_empty() {
                return Err(embedding_usage_args_error(args.len()));
            }

            let client = crate::extensions::context::tikv_client()
                .ok_or_else(|| anyhow!("embedding_usage: tikv client not available"))?;
            let db_id = crate::session_context::current_database_id();

            crate::extensions::embedding::check_embedding_installed(
                &client,
                db_id,
                "extensions.embedding_usage()",
            )
            .await?;

            if !crate::extensions::context::is_superuser() {
                return Err(embedding_usage_permission_error());
            }

            let (used, resets_at) =
                crate::extensions::embedding::read_embedding_usage(&client, db_id).await?;
            let resets_at_ms = parse_iso8601_to_epoch_ms(&resets_at)?;
            let mut schema = crate::extensions::embedding::embedding_usage_table_schema();
            apply_table_function_alias(&mut schema, alias)?;
            return Ok(Some(ExtensionTableFunctionResult::Batch(
                schema,
                vec![Row::new(vec![
                    Value::Int64(used),
                    Value::Timestamp(resets_at_ms),
                ])],
            )));
        }

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
                                other.data_type().unwrap_or(crate::model::DataType::Text)
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
                                other.data_type().unwrap_or(crate::model::DataType::Text)
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
            if !crate::extensions::context::is_superuser() {
                return Err(crate::sql::error::SqlError::PermissionDenied {
                    object_type: "extension".into(),
                    object_name: "\"http\"".into(),
                }
                .into());
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

        #[cfg(feature = "parquet")]
        if func_name.eq_ignore_ascii_case("read_parquet") {
            let installed = self.store().get_extension(txn, db_id, "parquet").await?;
            match installed {
                Some(ext) if ext.enabled => {}
                _ => {
                    return Err(anyhow!(
                        "extension \"parquet\" is not installed. Run: CREATE EXTENSION parquet"
                    ));
                }
            }
            let url_arg = args.first().ok_or_else(|| {
                anyhow!("read_parquet() requires exactly 1 argument: read_parquet('url')")
            })?;
            let url_expr = extract_expr_arg(url_arg)?;
            let url = expect_text(eval_const_ast_expr(url_expr)?, "url")?;
            let (mut schema, stream) =
                crate::extensions::parquet::reader::open_row_stream(&url).await?;
            apply_table_function_alias(&mut schema, alias)?;
            use futures::TryStreamExt;
            let mut rows: Vec<Row> = Vec::new();
            futures::pin_mut!(stream);
            while let Some(values) = stream.try_next().await? {
                rows.push(Row::new(values));
            }
            if rows.len() > 1_000_000 {
                tracing::warn!(
                    "read_parquet() materialized {} rows in memory. For large files, use COPY FROM ... WITH (FORMAT parquet) instead.",
                    rows.len()
                );
            }
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
                    return Ok((
                        super::super::ExecuteResult::CreateExtension {
                            ext_name: ext_name.clone(),
                        },
                        false,
                    ));
                }
                return Err(anyhow!("extension \"{}\" already exists", ext_name));
            }

            validate_extension_runtime_requirements(
                &ext_name,
                crate::worker::get_system_store().is_some(),
            )?;

            let ext = InstalledExtension::new(desc);
            self.store().put_extension(txn, db_id, &ext).await?;

            if ext_name == "pg_cron" {
                self.store().set_cron_enabled(txn, db_id).await?;
            }

            Ok((
                super::super::ExecuteResult::CreateExtension {
                    ext_name: ext_name.clone(),
                },
                true,
            ))
        }
        .await;

        if is_autocommit {
            if result.is_ok() {
                session.commit().await?;
            } else {
                session.rollback().await?;
            }
        }

        if !is_autocommit {
            if let Ok((_, created)) = &result {
                if *created {
                    session.note_extension_created(&ext_name);
                }
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

        result.map(|(res, _created)| res)
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

            Ok((
                super::super::ExecuteResult::DropExtension {
                    ext_name: ext_name.clone(),
                },
                dropped,
            ))
        }
        .await;

        if is_autocommit {
            if result.is_ok() {
                session.commit().await?;
            } else {
                session.rollback().await?;
            }
        }

        if !is_autocommit {
            if let Ok((_, dropped)) = &result {
                if *dropped {
                    session.note_extension_dropped(&ext_name);
                }
            }
        }

        result.map(|(res, _dropped)| res)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::ast::Ident;

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

    #[test]
    fn test_parse_create_extension_vector_maps_to_registered_descriptor() {
        let (if_not_exists, name) =
            parse_create_extension_sql("CREATE EXTENSION IF NOT EXISTS vector;").unwrap();
        assert!(if_not_exists);
        assert_eq!(name, "vector");

        let desc = crate::extensions::descriptor(&name)
            .expect("vector bootstrap SQL must map to a registered extension descriptor");
        assert_eq!(desc.name, "vector");
        assert_eq!(desc.default_schema, "public");
    }

    #[test]
    fn embedding_usage_argument_error_has_sqlstate_42883() {
        let err = embedding_usage_args_error(1);
        let sql_err = err
            .downcast_ref::<SqlError>()
            .expect("error must downcast to SqlError");
        assert_eq!(sql_err.sqlstate(), "42883");
        assert_eq!(
            sql_err.to_string(),
            "function extensions.embedding_usage(unknown) does not exist"
        );
    }

    #[test]
    fn embedding_usage_argument_error_formats_signature_for_multi_args() {
        let err = embedding_usage_args_error(3);
        let sql_err = err
            .downcast_ref::<SqlError>()
            .expect("error must downcast to SqlError");
        assert_eq!(sql_err.sqlstate(), "42883");
        assert_eq!(
            sql_err.to_string(),
            "function extensions.embedding_usage(unknown, unknown, unknown) does not exist"
        );
    }

    #[test]
    fn embedding_usage_zero_arity_signature_text() {
        let err = embedding_usage_args_error(0);
        let sql_err = err
            .downcast_ref::<SqlError>()
            .expect("error must downcast to SqlError");
        assert_eq!(sql_err.sqlstate(), "42883");
        assert_eq!(
            sql_err.to_string(),
            "function extensions.embedding_usage() does not exist"
        );
    }

    #[test]
    fn embedding_usage_permission_error_has_sqlstate_42501() {
        let err = embedding_usage_permission_error();
        let sql_err = err
            .downcast_ref::<SqlError>()
            .expect("error must downcast to SqlError");
        assert_eq!(sql_err.sqlstate(), "42501");
        assert_eq!(
            sql_err.to_string(),
            "permission denied for function embedding_usage"
        );
    }

    #[test]
    fn parse_iso8601_to_epoch_ms_accepts_valid_utc_timestamp() {
        let ms = parse_iso8601_to_epoch_ms("2026-03-05T00:00:00Z").expect("must parse");
        assert_eq!(ms, 1_772_668_800_000);
    }

    #[test]
    fn parse_iso8601_to_epoch_ms_rejects_invalid_timestamp() {
        let err = parse_iso8601_to_epoch_ms("not-a-timestamp").unwrap_err();
        assert!(err
            .to_string()
            .contains("embedding_usage: invalid resets_at timestamp"));
    }

    #[test]
    fn apply_table_function_alias_renames_columns_for_embedding_usage() {
        let mut schema = crate::extensions::embedding::embedding_usage_table_schema();
        let alias = TableAlias {
            name: Ident::new("u"),
            columns: vec![Ident::new("used"), Ident::new("reset_at")],
        };
        apply_table_function_alias(&mut schema, Some(&alias)).expect("alias should apply");
        assert_eq!(schema.columns[0].name, "used");
        assert_eq!(schema.columns[1].name, "reset_at");
    }

    #[test]
    fn apply_table_function_alias_rejects_mismatched_column_count() {
        let mut schema = crate::extensions::embedding::embedding_usage_table_schema();
        let alias = TableAlias {
            name: Ident::new("u"),
            columns: vec![Ident::new("only_one")],
        };
        let err = apply_table_function_alias(&mut schema, Some(&alias)).unwrap_err();
        assert!(err
            .to_string()
            .contains("Table function alias column count mismatch: expected 2, got 1"));
    }

    #[test]
    fn validate_extension_runtime_requirements_rejects_pg_cron_without_worker() {
        let err = validate_extension_runtime_requirements("pg_cron", false)
            .expect_err("pg_cron install must fail when worker is disabled");
        let msg = err.to_string();
        assert!(msg.contains("CREATE EXTENSION pg_cron requires the worker subsystem"));
        assert!(msg.contains("DB9_WORKER_ENABLED=false"));
    }

    #[test]
    fn validate_extension_runtime_requirements_allows_pg_cron_with_worker() {
        validate_extension_runtime_requirements("pg_cron", true)
            .expect("pg_cron install should pass when worker is available");
    }

    #[test]
    fn validate_extension_runtime_requirements_allows_other_extensions_without_worker() {
        validate_extension_runtime_requirements("http", false)
            .expect("non-cron extensions must remain installable without worker");
    }
}
