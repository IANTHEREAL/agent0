//! COPY protocol handling for [`DynamicPgHandler`].
//!
//! Contains `parse_copy_command`, `parse_copy_to_command`, COPY execution
//! helpers, and the [`CopyHandler`] trait implementation.

use super::DynamicPgHandler;
use crate::sql::error::SqlError;
use crate::sql::ExecuteResult;
use crate::types::{DataType, Value};
use async_trait::async_trait;
use futures::{Sink, SinkExt};
use pgwire::api::copy::CopyHandler;
use pgwire::api::results::CopyResponse;
use pgwire::api::ClientInfo;
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::copy::{CopyData, CopyDone, CopyFail};
use pgwire::messages::response::CommandComplete;
use pgwire::messages::PgWireBackendMessage;
use sqlparser::ast::{CopySource, CopyTarget, Ident, Statement};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use std::fmt::Debug;
use tracing::{error, warn};

use super::super::copy::copy_row_column_mismatch_error;
use super::super::errors::{
    error_info, in_failed_sql_transaction_pgwire_error, sqlstate_for_executor_error, user_error,
};
use super::super::query_parser::strip_leading_whitespace_and_comments;
use super::super::rollback_autocommit_or_mark_failed;
use crate::sql::Executor;

impl DynamicPgHandler {
    pub(in crate::protocol::handler) fn parse_copy_command(
        query: &str,
    ) -> Option<(String, Vec<String>)> {
        let query = strip_leading_whitespace_and_comments(query)?;
        let query_upper = query.to_uppercase();
        // COPY FROM STDIN must start at statement start (after leading whitespace/comments).
        if !query_upper.starts_with("COPY")
            || !query_upper.contains("FROM")
            || !query_upper.contains("STDIN")
        {
            return None;
        }

        // Single regex: COPY [schema.]table_name [(col1, col2, ...)] FROM stdin
        let re = regex::Regex::new(
            r"(?i)^COPY\s+(?:(\w+)\.)?(\w+)\s*(?:\(([^)]+)\)\s+|\s+)FROM\s+stdin",
        )
        .ok()?;
        let caps = re.captures(query)?;
        let schema = caps.get(1).map(|m| m.as_str().to_string());
        let table = caps.get(2)?.as_str().to_string();
        let table_name = match schema {
            Some(s) => format!("{}.{}", s, table),
            None => table,
        };
        let columns = match caps.get(3) {
            Some(cols) => cols
                .as_str()
                .split(',')
                .map(|s| s.trim().to_string())
                .collect(),
            None => vec![],
        };
        Some((table_name, columns))
    }

    #[allow(clippy::result_large_err)]
    pub(in crate::protocol::handler) fn parse_copy_to_command(
        query: &str,
    ) -> Result<
        Option<(
            String,
            Vec<String>,
            crate::protocol::copy_format::CopyOptions,
        )>,
        ErrorInfo,
    > {
        fn unsupported_copy_to_stdout_syntax() -> ErrorInfo {
            error_info(
                "0A000",
                "Unsupported COPY TO STDOUT syntax. Supported: COPY [schema.]table [(col1, col2, ...)] TO STDOUT [WITH (options)]",
            )
        }

        fn is_valid_unquoted_ident(ident: &str) -> bool {
            let mut chars = ident.chars();
            let Some(first) = chars.next() else {
                return false;
            };
            if first != '_' && !first.is_ascii_alphabetic() {
                return false;
            }
            chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
        }

        let Some(query_trimmed) = strip_leading_whitespace_and_comments(query) else {
            return Ok(None);
        };

        match query_trimmed.get(..4) {
            Some(prefix) if prefix.eq_ignore_ascii_case("COPY") => {}
            _ => return Ok(None),
        }

        let dialect = PostgreSqlDialect {};
        let Ok(stmts) = Parser::parse_sql(&dialect, query_trimmed) else {
            return Ok(None);
        };
        let Some(stmt) = stmts.first() else {
            return Ok(None);
        };

        let Statement::Copy {
            source,
            to,
            target,
            options,
            legacy_options,
            values,
        } = stmt
        else {
            return Ok(None);
        };

        if !*to || !matches!(target, CopyTarget::Stdout) {
            return Ok(None);
        }

        if stmts.len() != 1 || !legacy_options.is_empty() || !values.is_empty() {
            return Err(unsupported_copy_to_stdout_syntax());
        }

        let copy_opts = crate::protocol::copy_format::CopyOptions::from_copy_options(options)
            .map_err(|e| error_info("0A000", e))?;

        if copy_opts.format == crate::protocol::copy_format::CopyFormat::Parquet {
            return Err(error_info(
                "0A000",
                "COPY TO with FORMAT parquet is not supported",
            ));
        }

        let CopySource::Table {
            table_name,
            columns,
        } = source
        else {
            return Err(unsupported_copy_to_stdout_syntax());
        };

        let (schema_ident, table_ident) = match table_name.0.as_slice() {
            [table] => (None, table),
            [schema, table] => (Some(schema), table),
            _ => return Err(unsupported_copy_to_stdout_syntax()),
        };

        let validate_ident = |ident: &Ident| -> Result<(), ErrorInfo> {
            if ident.quote_style.is_some() {
                return Err(unsupported_copy_to_stdout_syntax());
            }
            if !is_valid_unquoted_ident(&ident.value) {
                return Err(error_info(
                    "42602",
                    format!("Invalid identifier in COPY TO STDOUT: \"{}\"", ident.value),
                ));
            }
            Ok(())
        };

        if let Some(schema) = schema_ident {
            validate_ident(schema)?;
        }
        validate_ident(table_ident)?;
        for col in columns.iter() {
            validate_ident(col)?;
        }

        let table_name = match schema_ident {
            Some(schema) => format!("{}.{}", schema.value, table_ident.value),
            None => table_ident.value.clone(),
        };
        let columns = columns.iter().map(|c| c.value.clone()).collect();

        Ok(Some((table_name, columns, copy_opts)))
    }

    #[allow(clippy::result_large_err)]
    fn copy_out_response_from_select_result(
        result: ExecuteResult,
    ) -> Result<(CopyResponse, Vec<String>, Vec<crate::types::Row>), ErrorInfo> {
        match result {
            ExecuteResult::Select { columns, rows, .. } => {
                let col_count = columns.len();
                let column_formats: Vec<i16> = vec![0; col_count];
                Ok((
                    CopyResponse::new(0, col_count, column_formats),
                    columns,
                    rows,
                ))
            }
            ExecuteResult::SelectStream { .. } => Err(error_info(
                "0A000",
                "streaming result cannot be used in COPY context",
            )),
            _ => Err(error_info(
                "0A000",
                "COPY TO STDOUT is only supported for tables",
            )),
        }
    }

    pub(in crate::protocol::handler) async fn handle_copy_to_stdout<'a, C>(
        &self,
        client: &mut C,
        table_name: &str,
        columns: &[String],
        copy_opts: &crate::protocol::copy_format::CopyOptions,
    ) -> PgWireResult<Vec<pgwire::api::results::Response<'a>>>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let state = self.auth();
        let executor = &state.executor;

        let select_sql = if columns.is_empty() {
            format!("SELECT * FROM {}", table_name)
        } else {
            format!("SELECT {} FROM {}", columns.join(", "), table_name)
        };

        let mut session = state.session.lock().await;

        let result = executor
            .execute(&mut session, &select_sql)
            .await
            .map(|r| r.last())
            .map_err(|e| user_error("XX000", e.to_string()))?;

        let (copy_resp, col_names, rows) = Self::copy_out_response_from_select_result(result)
            .map_err(|e| PgWireError::UserError(Box::new(e)))?;

        drop(session);

        pgwire::api::copy::send_copy_out_response(client, copy_resp).await?;

        let mut buf = Vec::with_capacity(4096);

        // Emit HEADER row if requested, encoding through format-specific logic
        // so column names containing delimiter/quote/newline are handled correctly.
        if copy_opts.header {
            let header_values: Vec<crate::types::Value> = col_names
                .iter()
                .map(|name| crate::types::Value::Text(name.clone()))
                .collect();
            crate::protocol::copy_format::encode_row_with_options(
                &header_values,
                &mut buf,
                copy_opts,
            )
            .map_err(|e| user_error("XX000", e.to_string()))?;
            let data = pgwire::messages::copy::CopyData::new(bytes::Bytes::copy_from_slice(&buf));
            client.send(PgWireBackendMessage::CopyData(data)).await?;
        }

        for row in &rows {
            buf.clear();
            crate::protocol::copy_format::encode_row_with_options(&row.values, &mut buf, copy_opts)
                .map_err(|e| user_error("XX000", e.to_string()))?;
            let data = pgwire::messages::copy::CopyData::new(bytes::Bytes::copy_from_slice(&buf));
            client.send(PgWireBackendMessage::CopyData(data)).await?;
        }

        let done = pgwire::messages::copy::CopyDone::new();
        client.send(PgWireBackendMessage::CopyDone(done)).await?;

        let complete =
            pgwire::messages::response::CommandComplete::new(format!("COPY {}", rows.len()));
        client
            .send(PgWireBackendMessage::CommandComplete(complete))
            .await?;

        Ok(vec![])
    }
}

#[cfg(feature = "parquet")]
impl DynamicPgHandler {
    /// Handle `COPY table FROM 'fs9://...' WITH (FORMAT csv|text)`.
    /// Returns `Ok(None)` for non-fs9 paths or FORMAT parquet (fall through).
    pub(in crate::protocol::handler) async fn try_handle_copy_from_fs9<'a, C>(
        &self,
        _client: &mut C,
        query: &str,
    ) -> PgWireResult<Option<Vec<pgwire::api::results::Response<'a>>>>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let stmts = match crate::sql::parse_sql(query) {
            Ok(s) if !s.is_empty() => s,
            _ => return Ok(None),
        };

        let (table_name, filename, copy_opts) = match &stmts[0] {
            Statement::Copy {
                source,
                to: false,
                target: CopyTarget::File { filename },
                options,
                ..
            } => {
                if !crate::extensions::parquet::reader::is_fs9_url(filename) {
                    return Ok(None);
                }

                let opts = crate::protocol::copy_format::CopyOptions::from_copy_options(options)
                    .map_err(|e| {
                        PgWireError::UserError(Box::new(ErrorInfo::new(
                            "ERROR".to_string(),
                            "0A000".to_string(),
                            e,
                        )))
                    })?;

                if opts.format == crate::protocol::copy_format::CopyFormat::Parquet {
                    return Ok(None);
                }

                let tbl = match source {
                    CopySource::Table {
                        table_name,
                        columns,
                    } => {
                        if !columns.is_empty() {
                            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                                "ERROR".to_string(),
                                "0A000".to_string(),
                                "COPY FROM fs9:// does not support column lists".to_string(),
                            ))));
                        }
                        table_name.to_string()
                    }
                    _ => return Ok(None),
                };

                (tbl, filename.clone(), opts)
            }
            _ => return Ok(None),
        };

        // --- session setup (mirrors try_handle_copy_from_parquet) ---
        let state = self.auth();
        let executor = &state.executor;
        let mut session = state.session.lock().await;

        if session.is_transaction_failed() {
            return Err(in_failed_sql_transaction_pgwire_error());
        }

        let started_txn = !session.is_in_transaction();
        if started_txn {
            session.begin().await.map_err(|e| {
                PgWireError::UserError(Box::new(ErrorInfo::new(
                    "ERROR".to_string(),
                    "XX000".to_string(),
                    e.to_string(),
                )))
            })?;
        }

        // INSERT privilege check
        {
            let current_role = session.current_user().map(|s| s.to_string());
            let txn = session.get_mut_txn().ok_or_else(|| {
                PgWireError::UserError(Box::new(ErrorInfo::new(
                    "ERROR".to_string(),
                    "XX000".to_string(),
                    "No transaction".to_string(),
                )))
            })?;
            executor
                .require_table_privilege(
                    txn,
                    current_role.as_deref(),
                    crate::auth::Privilege::Insert,
                    &table_name,
                )
                .await
                .map_err(|e| {
                    PgWireError::UserError(Box::new(ErrorInfo::new(
                        "ERROR".to_string(),
                        "42501".to_string(),
                        e.to_string(),
                    )))
                })?;
        }

        // Resolve table schema (search_path-aware, same as parquet handler)
        let db_id = session.current_database_id();
        let search_path: Vec<String> = session.search_path().to_vec();
        let (resolved_table, table_schema) = {
            let txn = session.get_mut_txn().ok_or_else(|| {
                PgWireError::UserError(Box::new(ErrorInfo::new(
                    "ERROR".to_string(),
                    "XX000".to_string(),
                    "No transaction".to_string(),
                )))
            })?;

            let normalize_ident = |s: &str| -> String {
                let trimmed = s.trim();
                if trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() > 1 {
                    trimmed[1..trimmed.len() - 1].to_string()
                } else {
                    trimmed.to_lowercase()
                }
            };
            let (schema_opt, table_ident) = if table_name.contains('.') {
                let parts: Vec<&str> = table_name.splitn(2, '.').collect();
                if parts.len() == 2 {
                    (Some(normalize_ident(parts[0])), normalize_ident(parts[1]))
                } else {
                    (None, normalize_ident(&table_name))
                }
            } else {
                (None, normalize_ident(&table_name))
            };

            if let Some(schema_ident) = schema_opt {
                let resolved = format!("{}.{}", schema_ident, table_ident);
                match executor.store().get_schema(txn, db_id, &resolved).await {
                    Ok(Some(schema)) => (resolved, schema),
                    Ok(None) => {
                        if started_txn {
                            let _ = session.rollback().await;
                        }
                        return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                            "ERROR".to_string(),
                            "42P01".to_string(),
                            format!("relation \"{}\" does not exist", table_name),
                        ))));
                    }
                    Err(e) => {
                        if started_txn {
                            let _ = session.rollback().await;
                        }
                        return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                            "ERROR".to_string(),
                            "XX000".to_string(),
                            e.to_string(),
                        ))));
                    }
                }
            } else {
                let schemas: Vec<&str> = if search_path.is_empty() {
                    vec!["public"]
                } else {
                    search_path.iter().map(|s| s.as_str()).collect()
                };
                let mut found = None;
                for s in schemas {
                    let resolved = format!("{}.{}", s, table_ident);
                    match executor.store().get_schema(txn, db_id, &resolved).await {
                        Ok(Some(schema)) => {
                            found = Some((resolved, schema));
                            break;
                        }
                        Ok(None) => continue,
                        Err(e) => {
                            if started_txn {
                                let _ = session.rollback().await;
                            }
                            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                                "ERROR".to_string(),
                                "XX000".to_string(),
                                e.to_string(),
                            ))));
                        }
                    }
                }
                match found {
                    Some(f) => f,
                    None => {
                        if started_txn {
                            let _ = session.rollback().await;
                        }
                        return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                            "ERROR".to_string(),
                            "42P01".to_string(),
                            format!("relation \"{}\" does not exist", table_name),
                        ))));
                    }
                }
            }
        };

        // Enforce fs9 local-filesystem permission (same gate as fs9 table function)
        let use_remote = crate::extensions::fs::backend::is_remote_configured();
        if !use_remote && !session.is_superuser() {
            if started_txn {
                let _ = session.rollback().await;
            }
            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                "ERROR".to_string(),
                "42501".to_string(),
                "COPY FROM fs9: local filesystem access denied (requires superuser)".to_string(),
            ))));
        }

        // Read file bytes from fs9 backend
        let bare_path = crate::extensions::parquet::reader::strip_fs9_scheme(&filename);
        let tenant = executor.tenant_keyspace().to_string();
        let backend = crate::extensions::fs::backend::get_backend(&tenant);
        let file_data = match backend
            .read_file(
                bare_path,
                crate::extensions::parquet::fs9_reader::MAX_FS9_PARQUET_FILE_BYTES,
            )
            .await
        {
            Ok(data) => data,
            Err(e) => {
                if started_txn {
                    let _ = session.rollback().await;
                }
                return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                    "ERROR".to_string(),
                    "58030".to_string(),
                    format!("COPY FROM fs9: {}", e),
                ))));
            }
        };

        // Prepare column metadata from table schema
        let column_names: Vec<String> = table_schema
            .columns
            .iter()
            .map(|c| c.name.clone())
            .collect();
        let column_types: Vec<Option<crate::types::DataType>> = table_schema
            .columns
            .iter()
            .map(|c| Some(c.data_type.clone()))
            .collect();

        let short_table = resolved_table.rsplit('.').next().unwrap_or(&resolved_table);

        // Parse records: CSV format uses csv crate (handles quoting),
        // TEXT format uses parse_copy_text_line (handles backslash escapes).
        let is_csv = copy_opts.format == crate::protocol::copy_format::CopyFormat::Csv;
        let records: Vec<Vec<(String, Value)>> = if is_csv {
            let mut builder = csv::ReaderBuilder::new();
            builder
                .delimiter(copy_opts.delimiter)
                .has_headers(copy_opts.header)
                .flexible(true)
                .quote(copy_opts.quote);
            if copy_opts.escape == copy_opts.quote {
                builder.double_quote(true);
            } else {
                builder.double_quote(false).escape(Some(copy_opts.escape));
            }
            let mut rdr = builder.from_reader(file_data.as_slice());
            let mut rows = Vec::new();
            for (rec_idx, record_result) in rdr.records().enumerate() {
                let record = record_result.map_err(|e| {
                    PgWireError::UserError(Box::new(ErrorInfo::new(
                        "ERROR".to_string(),
                        "22P04".to_string(),
                        format!("CSV parse error at line {}: {}", rec_idx + 1, e),
                    )))
                })?;
                let mut col_values = Vec::with_capacity(column_names.len());
                for (idx, (col_name, col_type)) in
                    column_names.iter().zip(column_types.iter()).enumerate()
                {
                    let val = record.get(idx).unwrap_or("");
                    let value = if val == copy_opts.null_string {
                        Value::Null
                    } else if let Some(dt) = col_type.as_ref() {
                        executor.parse_value_for_copy(val, dt).map_err(|e| {
                            PgWireError::UserError(Box::new(ErrorInfo::new(
                                "ERROR".to_string(),
                                "22P02".to_string(),
                                format!(
                                    "{}\nCONTEXT:  COPY {}, line {}, column {}: \"{}\"",
                                    e,
                                    short_table,
                                    rec_idx + 1,
                                    col_name,
                                    val
                                ),
                            )))
                        })?
                    } else {
                        Value::Text(val.to_string())
                    };
                    col_values.push((col_name.clone(), value));
                }
                rows.push(col_values);
            }
            rows
        } else {
            // TEXT format: split by newline, reuse parse_copy_text_line
            let text = String::from_utf8_lossy(&file_data);
            let mut lines: Vec<&str> = text.lines().collect();
            if copy_opts.header && !lines.is_empty() {
                lines.remove(0);
            }
            if lines.last().is_some_and(|l| l.is_empty()) {
                lines.pop();
            }
            let mut rows = Vec::with_capacity(lines.len());
            for (line_idx, line) in lines.iter().enumerate() {
                let line_no = line_idx + 1;
                let col_values = parse_copy_text_line(
                    executor,
                    &resolved_table,
                    &column_names,
                    &column_types,
                    line_no,
                    line.as_bytes(),
                )?;
                rows.push(col_values);
            }
            rows
        };

        // Insert all parsed rows within QueryContext scope
        let statement_ts = chrono::Utc::now().timestamp_millis();
        let transaction_ts = session.transaction_timestamp_ms().unwrap_or(statement_ts);
        let qctx = session.query_context_for_statement(statement_ts, transaction_ts);
        let row_count = records.len();

        let insert_result: PgWireResult<()> =
            crate::sql::query_context::with_scoped_query_context(&qctx, async {
                for (rec_idx, col_values) in records.into_iter().enumerate() {
                    let line_no = rec_idx + 1;
                    executor
                        .execute_copy_insert(session, &resolved_table, col_values)
                        .await
                        .map_err(|e| {
                            let message = if should_add_copy_insert_context(&e) {
                                format!("{}\nCONTEXT:  COPY {}, line {}", e, short_table, line_no)
                            } else {
                                e.to_string()
                            };
                            PgWireError::UserError(Box::new(ErrorInfo::new(
                                "ERROR".to_string(),
                                sqlstate_for_executor_error(&e).to_string(),
                                message,
                            )))
                        })?;
                }
                Ok(())
            })
            .await;

        if let Err(e) = insert_result {
            if started_txn {
                let _ = session.rollback().await;
            } else {
                session.mark_transaction_failed();
            }
            return Err(e);
        }

        if started_txn {
            session.commit().await.map_err(|e| {
                PgWireError::UserError(Box::new(ErrorInfo::new(
                    "ERROR".to_string(),
                    "XX000".to_string(),
                    e.to_string(),
                )))
            })?;
        }

        Ok(Some(vec![pgwire::api::results::Response::Execution(
            pgwire::api::results::Tag::new("COPY").with_rows(row_count),
        )]))
    }

    pub(in crate::protocol::handler) async fn try_handle_copy_from_parquet<'a, C>(
        &self,
        _client: &mut C,
        query: &str,
    ) -> PgWireResult<Option<Vec<pgwire::api::results::Response<'a>>>>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let stmts = match crate::sql::parse_sql(query) {
            Ok(s) if !s.is_empty() => s,
            _ => return Ok(None),
        };

        let (table_name, url) = match &stmts[0] {
            Statement::Copy {
                source,
                to: false,
                target: CopyTarget::File { filename },
                options,
                ..
            } => {
                let is_parquet = options.iter().any(|opt| {
                    matches!(opt, sqlparser::ast::CopyOption::Format(ident)
                        if ident.value.eq_ignore_ascii_case("parquet"))
                });
                if !is_parquet {
                    return Ok(None);
                }

                let table_name = match source {
                    CopySource::Table {
                        table_name,
                        columns,
                    } => {
                        if !columns.is_empty() {
                            return Err(user_error(
                                "0A000",
                                "COPY FROM with FORMAT parquet does not support column lists",
                            ));
                        }
                        table_name.to_string()
                    }
                    _ => return Ok(None),
                };

                // Reject options that are not applicable to Parquet format
                for opt in options {
                    match opt {
                        sqlparser::ast::CopyOption::Format(_) => {} // already handled
                        other => {
                            return Err(user_error(
                                "0A000",
                                format!("option {:?} is not supported with FORMAT parquet", other),
                            ));
                        }
                    }
                }

                (table_name, filename.clone())
            }
            Statement::Copy {
                to: false,
                target: CopyTarget::Stdin,
                options,
                ..
            } => {
                let is_parquet = options.iter().any(|opt| {
                    matches!(opt, sqlparser::ast::CopyOption::Format(ident)
                        if ident.value.eq_ignore_ascii_case("parquet"))
                });
                if is_parquet {
                    return Err(user_error("0A000", "Parquet format does not support STDIN"));
                }
                return Ok(None);
            }
            _ => return Ok(None),
        };

        let state = self.auth();
        let executor = &state.executor;
        let mut session = state.session.lock().await;

        if session.is_transaction_failed() {
            return Err(in_failed_sql_transaction_pgwire_error());
        }

        let started_txn = !session.is_in_transaction();
        if started_txn {
            session
                .begin()
                .await
                .map_err(|e| user_error("XX000", e.to_string()))?;
        }

        let db_id = session.current_database_id();

        // Check extension is installed
        {
            let txn = session
                .get_mut_txn()
                .ok_or_else(|| user_error("XX000", "No transaction"))?;
            let installed = executor
                .store()
                .get_extension(txn, db_id, "parquet")
                .await
                .map_err(|e| user_error("XX000", e.to_string()))?;
            match installed {
                Some(ref ext) if ext.enabled => {}
                _ => {
                    if started_txn {
                        let _ = session.rollback().await;
                    }
                    return Err(user_error(
                        "0A000",
                        "extension \"parquet\" is not installed. Run: CREATE EXTENSION parquet",
                    ));
                }
            }
        }

        // Require INSERT privilege (same as COPY FROM STDIN)
        {
            let current_role = session.current_user().map(|s| s.to_string());
            let txn = session
                .get_mut_txn()
                .ok_or_else(|| user_error("XX000", "No transaction"))?;
            executor
                .require_table_privilege(
                    txn,
                    current_role.as_deref(),
                    crate::auth::Privilege::Insert,
                    &table_name,
                )
                .await
                .map_err(|e| user_error("42501", e.to_string()))?;
        }

        // Set up statement context (timestamps, connection_id, etc.) required by
        // QueryContext::from_task_locals() inside execute_copy_from_parquet.
        let statement_ts = chrono::Utc::now().timestamp_millis();
        let transaction_ts = session.transaction_timestamp_ms().unwrap_or(statement_ts);
        let qctx = session.query_context_for_statement(statement_ts, transaction_ts);

        // Delegate streaming import to Executor (has access to dml/check_constraints modules).
        // Extension context is required for fs9:// URL dispatch in open_batch_stream.
        let tenant_ks = executor.tenant_keyspace().to_string();
        let is_super = session.is_superuser();
        let result = crate::sql::query_context::with_scoped_query_context(&qctx, async {
            crate::extensions::context::with_context(is_super, &tenant_ks, async {
                executor
                    .execute_copy_from_parquet(session, &table_name, &url)
                    .await
            })
            .await
        })
        .await;

        if let Err(e) = result {
            if started_txn {
                let _ = session.rollback().await;
            } else {
                session.mark_transaction_failed();
            }
            return Err(user_error("XX000", e.to_string()));
        }
        let result = result.unwrap();

        if started_txn {
            session
                .commit()
                .await
                .map_err(|e| user_error("XX000", e.to_string()))?;
        }

        Ok(Some(vec![pgwire::api::results::Response::Execution(
            pgwire::api::results::Tag::new("COPY").with_rows(result),
        )]))
    }
}

fn copy_display_table_name(resolved_table: &str) -> &str {
    resolved_table.rsplit('.').next().unwrap_or(resolved_table)
}

fn copy_missing_data_error(
    resolved_table: &str,
    column_name: &str,
    line_no: usize,
    raw_line: &str,
) -> PgWireError {
    let table = copy_display_table_name(resolved_table);
    user_error(
        "22P04",
        format!(
            "missing data for column \"{}\"\nCONTEXT:  COPY {}, line {}: \"{}\"",
            column_name, table, line_no, raw_line
        ),
    )
}

fn copy_value_parse_error(
    resolved_table: &str,
    line_no: usize,
    column_name: &str,
    raw_value: &str,
    err: &anyhow::Error,
) -> PgWireError {
    let table = copy_display_table_name(resolved_table);
    user_error(
        sqlstate_for_executor_error(err),
        format!(
            "{}\nCONTEXT:  COPY {}, line {}, column {}: \"{}\"",
            err, table, line_no, column_name, raw_value
        ),
    )
}

fn parse_copy_text_line(
    executor: &Executor,
    resolved_table: &str,
    columns: &[String],
    column_types: &[Option<DataType>],
    line_no: usize,
    line_bytes: &[u8],
) -> PgWireResult<Vec<(String, Value)>> {
    let line = String::from_utf8_lossy(line_bytes);
    let values: Vec<&str> = line.split('\t').collect();

    if values.len() > columns.len() {
        return Err(copy_row_column_mismatch_error(values.len(), columns.len()));
    }

    let mut col_values: Vec<(String, Value)> = Vec::with_capacity(columns.len());
    for (idx, (col_name, col_type)) in columns.iter().zip(column_types.iter()).enumerate() {
        let Some(val) = values.get(idx).copied() else {
            return Err(copy_missing_data_error(
                resolved_table,
                col_name,
                line_no,
                line.as_ref(),
            ));
        };

        let value = if val == "\\N" {
            Value::Null
        } else if let Some(dt) = col_type.as_ref() {
            executor
                .parse_value_for_copy(val, dt)
                .map_err(|e| copy_value_parse_error(resolved_table, line_no, col_name, val, &e))?
        } else {
            Value::Text(val.to_string())
        };
        col_values.push((col_name.clone(), value));
    }

    Ok(col_values)
}

fn should_add_copy_insert_context(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<SqlError>(),
        Some(SqlError::UniqueViolation { .. })
    )
}

#[async_trait]
impl CopyHandler for DynamicPgHandler {
    async fn on_copy_data<C>(&self, _client: &mut C, copy_data: CopyData) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let executor = &self.auth().executor;

        let (parse_res, table_name, started_txn, qctx) = {
            let mut ctx_guard = self.copy_context.lock().await;
            let Some(ctx) = ctx_guard.as_mut() else {
                return Ok(());
            };

            let table_name = ctx.table_name.clone();
            let started_txn = ctx.started_txn;
            let qctx = ctx.query_context.clone();

            let parse_res = (|| -> PgWireResult<Vec<(usize, Vec<(String, Value)>)>> {
                if ctx.reached_end_marker {
                    return Ok(Vec::new());
                }

                let lines = ctx.push_copy_data(copy_data.data.as_ref())?;
                if lines.is_empty() {
                    return Ok(Vec::new());
                }

                let mut rows_to_insert: Vec<(usize, Vec<(String, Value)>)> =
                    Vec::with_capacity(lines.len());
                for line_bytes in lines {
                    if ctx.reached_end_marker {
                        break;
                    }
                    if line_bytes.as_slice() == b"\\." {
                        ctx.reached_end_marker = true;
                        ctx.line_buffer.clear();
                        break;
                    }

                    let line_no = ctx
                        .row_count
                        .saturating_add(rows_to_insert.len())
                        .saturating_add(1);
                    let col_values = parse_copy_text_line(
                        executor,
                        &ctx.table_name,
                        &ctx.columns,
                        &ctx.column_types,
                        line_no,
                        &line_bytes,
                    )?;
                    rows_to_insert.push((line_no, col_values));
                }

                Ok(rows_to_insert)
            })();

            (parse_res, table_name, started_txn, qctx)
        };

        let rows_to_insert = match parse_res {
            Ok(rows) => rows,
            Err(e) => {
                let mut ctx_guard = self.copy_context.lock().await;
                *ctx_guard = None;
                drop(ctx_guard);

                let mut session = self.auth().session.lock().await;
                rollback_autocommit_or_mark_failed(&mut session, started_txn).await;
                return Err(e);
            }
        };

        let inserted_count = rows_to_insert.len();
        if inserted_count == 0 {
            return Ok(());
        }

        let insert_res: PgWireResult<()> =
            crate::sql::query_context::with_scoped_query_context(&qctx, async {
                let mut session = self.auth().session.lock().await;

                if session.is_transaction_failed() {
                    return Err(in_failed_sql_transaction_pgwire_error());
                }

                let savepoints = session.savepoints();
                crate::txn::with_savepoints(savepoints, async {
                    for (line_no, col_values) in rows_to_insert {
                        executor
                            .execute_copy_insert(&mut session, &table_name, col_values)
                            .await
                            .map_err(|e| {
                                error!("COPY insert error: {}", e);
                                let table = copy_display_table_name(&table_name);
                                let message = if should_add_copy_insert_context(&e) {
                                    format!("{}\nCONTEXT:  COPY {}, line {}", e, table, line_no)
                                } else {
                                    e.to_string()
                                };
                                user_error(sqlstate_for_executor_error(&e), message)
                            })?;
                    }

                    Ok::<(), PgWireError>(())
                })
                .await?;

                Ok(())
            })
            .await;

        if let Err(e) = insert_res {
            let mut session = self.auth().session.lock().await;
            rollback_autocommit_or_mark_failed(&mut session, started_txn).await;
            drop(session);

            let mut ctx_guard = self.copy_context.lock().await;
            *ctx_guard = None;
            return Err(e);
        }

        let mut ctx_guard = self.copy_context.lock().await;
        if let Some(ctx) = ctx_guard.as_mut() {
            ctx.row_count = ctx.row_count.saturating_add(inserted_count);
        }

        Ok(())
    }

    async fn on_copy_done<C>(&self, client: &mut C, _done: CopyDone) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let ctx_opt = {
            let mut ctx_guard = self.copy_context.lock().await;
            ctx_guard.take()
        };

        let row_count = if let Some(mut ctx) = ctx_opt {
            let executor = &self.auth().executor;
            let qctx = ctx.query_context.clone();

            crate::sql::query_context::with_scoped_query_context(&qctx, async {
                let mut session = self.auth().session.lock().await;

                if session.is_transaction_failed() {
                    return Err(in_failed_sql_transaction_pgwire_error());
                }

                if let Some(final_line_bytes) = ctx.drain_final_line() {
                    if final_line_bytes.as_slice() == b"\\." {
                        ctx.reached_end_marker = true;
                    } else if !ctx.reached_end_marker {
                        let line_no = ctx.row_count.saturating_add(1);
                        let col_values = match parse_copy_text_line(
                            executor,
                            &ctx.table_name,
                            &ctx.columns,
                            &ctx.column_types,
                            line_no,
                            &final_line_bytes,
                        ) {
                            Ok(values) => values,
                            Err(e) => {
                                rollback_autocommit_or_mark_failed(&mut session, ctx.started_txn)
                                    .await;
                                return Err(e);
                            }
                        };

                        let savepoints = session.savepoints();
                        let insert_res = crate::txn::with_savepoints(savepoints, async {
                            executor
                                .execute_copy_insert(&mut session, &ctx.table_name, col_values)
                                .await
                                .map_err(|e| {
                                    error!("COPY insert error: {}", e);
                                    let table = copy_display_table_name(&ctx.table_name);
                                    let message = if should_add_copy_insert_context(&e) {
                                        format!("{}\nCONTEXT:  COPY {}, line {}", e, table, line_no)
                                    } else {
                                        e.to_string()
                                    };
                                    user_error(sqlstate_for_executor_error(&e), message)
                                })
                        })
                        .await;

                        if let Err(e) = insert_res {
                            rollback_autocommit_or_mark_failed(&mut session, ctx.started_txn).await;
                            return Err(e);
                        }

                        ctx.row_count = ctx.row_count.saturating_add(1);
                    }
                }

                if ctx.started_txn {
                    session
                        .commit()
                        .await
                        .map_err(|e| user_error("XX000", e.to_string()))?;
                }

                Ok::<usize, PgWireError>(ctx.row_count)
            })
            .await?
        } else {
            0
        };

        client
            .send(PgWireBackendMessage::CommandComplete(CommandComplete::new(
                format!("COPY {}", row_count),
            )))
            .await?;

        Ok(())
    }

    async fn on_copy_fail<C>(&self, _client: &mut C, fail: CopyFail) -> PgWireError
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let ctx_opt = {
            let mut ctx_guard = self.copy_context.lock().await;
            ctx_guard.take()
        };

        if let Some(ctx) = ctx_opt {
            let mut session = self.auth().session.lock().await;
            rollback_autocommit_or_mark_failed(&mut session, ctx.started_txn).await;
        }

        warn!("COPY failed: {}", fail.message);

        user_error(
            "XX000",
            format!("COPY IN mode terminated: {}", fail.message),
        )
    }
}
