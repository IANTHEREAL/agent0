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
use super::super::errors::{in_failed_sql_transaction_pgwire_error, sqlstate_for_executor_error};
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

        // Regex: COPY [schema.]table_name (col1, col2, ...) FROM stdin
        let re = regex::Regex::new(r"(?i)^COPY\s+(?:(\w+)\.)?(\w+)\s*\(([^)]+)\)\s+FROM\s+stdin")
            .ok()?;
        if let Some(caps) = re.captures(query) {
            let schema = caps.get(1).map(|m| m.as_str().to_string());
            let table = caps.get(2)?.as_str().to_string();
            let table_name = match schema {
                Some(s) => format!("{}.{}", s, table),
                None => table,
            };
            let columns_str = caps.get(3)?.as_str();
            let columns: Vec<String> = columns_str
                .split(',')
                .map(|s| s.trim().to_string())
                .collect();
            return Some((table_name, columns));
        }

        // Regex: COPY [schema.]table_name FROM stdin (no column list)
        let re2 = regex::Regex::new(r"(?i)^COPY\s+(?:(\w+)\.)?(\w+)\s+FROM\s+stdin").ok()?;
        if let Some(caps) = re2.captures(query) {
            let schema = caps.get(1).map(|m| m.as_str().to_string());
            let table = caps.get(2)?.as_str().to_string();
            let table_name = match schema {
                Some(s) => format!("{}.{}", s, table),
                None => table,
            };
            return Some((table_name, vec![]));
        }

        None
    }

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
            ErrorInfo::new(
                "ERROR".to_string(),
                "0A000".to_string(),
                "Unsupported COPY TO STDOUT syntax. Supported: COPY [schema.]table [(col1, col2, ...)] TO STDOUT [WITH (options)]".to_string(),
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
            .map_err(|e| ErrorInfo::new("ERROR".to_string(), "0A000".to_string(), e))?;

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
                return Err(ErrorInfo::new(
                    "ERROR".to_string(),
                    "42602".to_string(),
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
            _ => Err(ErrorInfo::new(
                "ERROR".to_string(),
                "0A000".to_string(),
                "COPY TO STDOUT is only supported for tables".to_string(),
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
        let executor = self.get_executor()?;

        let select_sql = if columns.is_empty() {
            format!("SELECT * FROM {}", table_name)
        } else {
            format!("SELECT {} FROM {}", columns.join(", "), table_name)
        };

        let mut session_guard = self.session.lock().await;
        let session = session_guard.as_mut().ok_or_else(|| {
            PgWireError::UserError(Box::new(ErrorInfo::new(
                "ERROR".to_string(),
                "XX000".to_string(),
                "Session not initialized".to_string(),
            )))
        })?;

        let result = executor
            .execute(session, &select_sql)
            .await
            .map(|r| r.last())
            .map_err(|e| {
                PgWireError::UserError(Box::new(ErrorInfo::new(
                    "ERROR".to_string(),
                    "XX000".to_string(),
                    e.to_string(),
                )))
            })?;

        let (copy_resp, col_names, rows) = Self::copy_out_response_from_select_result(result)
            .map_err(|e| PgWireError::UserError(Box::new(e)))?;

        drop(session_guard);

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
            );
            let data = pgwire::messages::copy::CopyData::new(bytes::Bytes::copy_from_slice(&buf));
            client.send(PgWireBackendMessage::CopyData(data)).await?;
        }

        for row in &rows {
            buf.clear();
            crate::protocol::copy_format::encode_row_with_options(&row.values, &mut buf, copy_opts);
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
    PgWireError::UserError(Box::new(ErrorInfo::new(
        "ERROR".to_string(),
        "22P04".to_string(),
        format!(
            "missing data for column \"{}\"\nCONTEXT:  COPY {}, line {}: \"{}\"",
            column_name, table, line_no, raw_line
        ),
    )))
}

fn copy_value_parse_error(
    resolved_table: &str,
    line_no: usize,
    column_name: &str,
    raw_value: &str,
    err: &anyhow::Error,
) -> PgWireError {
    let table = copy_display_table_name(resolved_table);
    PgWireError::UserError(Box::new(ErrorInfo::new(
        "ERROR".to_string(),
        sqlstate_for_executor_error(err).to_string(),
        format!(
            "{}\nCONTEXT:  COPY {}, line {}, column {}: \"{}\"",
            err, table, line_no, column_name, raw_value
        ),
    )))
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
        let executor = self.get_executor()?;

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

                let mut session_guard = self.session.lock().await;
                if let Some(session) = session_guard.as_mut() {
                    rollback_autocommit_or_mark_failed(session, started_txn).await;
                }
                return Err(e);
            }
        };

        let inserted_count = rows_to_insert.len();
        if inserted_count == 0 {
            return Ok(());
        }

        let insert_res: PgWireResult<()> =
            crate::sql::query_context::with_scoped_query_context(&qctx, async {
                let mut session_guard = self.session.lock().await;
                let session = session_guard.as_mut().ok_or_else(|| {
                    PgWireError::UserError(Box::new(ErrorInfo::new(
                        "ERROR".to_string(),
                        "XX000".to_string(),
                        "Session not initialized".to_string(),
                    )))
                })?;

                if session.is_transaction_failed() {
                    return Err(in_failed_sql_transaction_pgwire_error());
                }

                let savepoints = session.savepoints();
                crate::txn::with_savepoints(savepoints, async {
                    for (line_no, col_values) in rows_to_insert {
                        executor
                            .execute_copy_insert(session, &table_name, col_values)
                            .await
                            .map_err(|e| {
                                error!("COPY insert error: {}", e);
                                let table = copy_display_table_name(&table_name);
                                let message = if should_add_copy_insert_context(&e) {
                                    format!("{}\nCONTEXT:  COPY {}, line {}", e, table, line_no)
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

                    Ok::<(), PgWireError>(())
                })
                .await?;

                Ok(())
            })
            .await;

        if let Err(e) = insert_res {
            let mut session_guard = self.session.lock().await;
            if let Some(session) = session_guard.as_mut() {
                rollback_autocommit_or_mark_failed(session, started_txn).await;
            }
            drop(session_guard);

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
            let executor = self.get_executor()?;
            let qctx = ctx.query_context.clone();

            crate::sql::query_context::with_scoped_query_context(&qctx, async {
                let mut session_guard = self.session.lock().await;
                let session = session_guard.as_mut().ok_or_else(|| {
                    PgWireError::UserError(Box::new(ErrorInfo::new(
                        "ERROR".to_string(),
                        "XX000".to_string(),
                        "Session not initialized".to_string(),
                    )))
                })?;

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
                                rollback_autocommit_or_mark_failed(session, ctx.started_txn).await;
                                return Err(e);
                            }
                        };

                        let savepoints = session.savepoints();
                        let insert_res = crate::txn::with_savepoints(savepoints, async {
                            executor
                                .execute_copy_insert(session, &ctx.table_name, col_values)
                                .await
                                .map_err(|e| {
                                    error!("COPY insert error: {}", e);
                                    let table = copy_display_table_name(&ctx.table_name);
                                    let message = if should_add_copy_insert_context(&e) {
                                        format!("{}\nCONTEXT:  COPY {}, line {}", e, table, line_no)
                                    } else {
                                        e.to_string()
                                    };
                                    PgWireError::UserError(Box::new(ErrorInfo::new(
                                        "ERROR".to_string(),
                                        sqlstate_for_executor_error(&e).to_string(),
                                        message,
                                    )))
                                })
                        })
                        .await;

                        if let Err(e) = insert_res {
                            rollback_autocommit_or_mark_failed(session, ctx.started_txn).await;
                            return Err(e);
                        }

                        ctx.row_count = ctx.row_count.saturating_add(1);
                    }
                }

                if ctx.started_txn {
                    session.commit().await.map_err(|e| {
                        PgWireError::UserError(Box::new(ErrorInfo::new(
                            "ERROR".to_string(),
                            "XX000".to_string(),
                            e.to_string(),
                        )))
                    })?;
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
            let mut session_guard = self.session.lock().await;
            if let Some(session) = session_guard.as_mut() {
                rollback_autocommit_or_mark_failed(session, ctx.started_txn).await;
            }
        }

        warn!("COPY failed: {}", fail.message);

        PgWireError::UserError(Box::new(ErrorInfo::new(
            "ERROR".to_owned(),
            "XX000".to_owned(),
            format!("COPY IN mode terminated: {}", fail.message),
        )))
    }
}
