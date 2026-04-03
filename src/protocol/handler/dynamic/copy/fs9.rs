//! fs9 remote COPY support (feature-gated on `parquet`).
//!
//! Contains `try_handle_copy_from_fs9` and `try_handle_copy_from_parquet`.

use super::super::super::errors::{
    in_failed_sql_transaction_pgwire_error, sqlstate_for_executor_error, user_error,
};
use super::super::super::rollback_autocommit_or_mark_failed;
use super::super::DynamicPgHandler;
use super::helpers::{parse_copy_text_line, should_add_copy_insert_context};
use crate::model::Value;
use futures::Sink;
use pgwire::api::ClientInfo;
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::PgWireBackendMessage;
use sqlparser::ast::{CopySource, CopyTarget, Statement};
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;

fn copy_from_fs9_io_error(err: impl std::fmt::Display) -> PgWireError {
    user_error("58030", format!("COPY FROM fs9: {}", err))
}

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

        if self.cancel_token.is_cancelled() {
            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                "FATAL".to_string(),
                "25P03".to_string(),
                "terminating connection due to idle-in-transaction timeout".to_string(),
            ))));
        }

        if let Err(e) = session.check_idle_in_transaction_timeout() {
            let _ = session.rollback().await;
            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                "FATAL".to_string(),
                e.sqlstate().to_string(),
                e.to_string(),
            ))));
        }

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

        // All fallible work after begin() is wrapped in an async block so that
        // every `?` is caught by the single cleanup site below.
        let result: PgWireResult<usize> = async {
            // Server-file privilege check first (PG checks file permission
            // before table privilege for COPY … FROM 'filename').
            // Without this, a non-superuser can probe table existence by
            // observing whether the error is "table not found" vs "permission
            // denied for table".
            if !session.is_superuser() {
                return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                    "ERROR".to_string(),
                    "42501".to_string(),
                    "permission denied to COPY from a file".to_string(),
                ))));
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
                            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                                "ERROR".to_string(),
                                "42P01".to_string(),
                                format!("relation \"{}\" does not exist", table_name),
                            ))));
                        }
                        Err(e) => {
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
                            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                                "ERROR".to_string(),
                                "42P01".to_string(),
                                format!("relation \"{}\" does not exist", table_name),
                            ))));
                        }
                    }
                }
            };

            let statement_ts = chrono::Utc::now().timestamp_millis();
            let transaction_ts = session.transaction_timestamp_ms().unwrap_or(statement_ts);
            let qctx = session.query_context_for_statement(statement_ts, transaction_ts);
            let runtime = crate::sql::runtime_context::StatementRuntimeContext::from_session(
                &session,
                executor.tenant_keyspace(),
                executor.store().transaction_client(),
            );

            // Read file bytes from fs9 backend under the same statement runtime
            // contract as the later COPY insert work, so extension context
            // (tenant keyspace + TiKV client) is present for fs9 IO.
            let file_data = super::with_copy_statement_context(&qctx, &runtime, async {
                if !crate::extensions::fs::backend::is_backend_available() {
                    return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                        "ERROR".to_string(),
                        "58030".to_string(),
                        "fs9: TiKV storage backend not available".to_string(),
                    ))));
                }
                // superuser check already done above (before table privilege)

                let bare_path = crate::extensions::parquet::reader::strip_fs9_scheme(&filename);
                let tenant = executor.tenant_keyspace().to_string();
                let backend = crate::extensions::fs::backend::acquire_statement_backend(&tenant)
                    .await
                    .map_err(copy_from_fs9_io_error)?;
                backend
                    .read_file(
                        bare_path,
                        crate::extensions::parquet::fs9_reader::MAX_FS9_PARQUET_FILE_BYTES,
                    )
                    .await
                    .map_err(copy_from_fs9_io_error)
            })
            .await?;

            // Prepare column metadata from table schema
            let column_names: Vec<String> = table_schema
                .columns
                .iter()
                .map(|c| c.name.clone())
                .collect();
            let column_types: Vec<Option<crate::model::DataType>> = table_schema
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
                            crate::protocol::copy_format::parse_value_for_copy(val, dt).map_err(
                                |e| {
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
                                },
                            )?
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
                        &resolved_table,
                        &column_names,
                        &column_types,
                        line_no,
                        line.as_bytes(),
                        &copy_opts,
                    )?;
                    rows.push(col_values);
                }
                rows
            };

            // Insert all parsed rows within the same statement runtime scope.
            let row_count = records.len();
            let line_numbers: Vec<usize> = (1..=row_count).collect();

            let mut pending_self_fk_keys: HashMap<String, HashSet<String>> = HashMap::new();
            let mut deferred_self_fk_checks: Vec<(usize, String, String, String)> = Vec::new();

            let insert_result: PgWireResult<()> =
                crate::sql::query_context::with_scoped_query_context(
                    &qctx,
                    crate::sql::runtime_context::wrap_with_statement_runtime_context(
                        &runtime,
                        async {
                            executor
                                .execute_copy_insert_batch(
                                    &mut session,
                                    &resolved_table,
                                    records,
                                    Some(&mut pending_self_fk_keys),
                                    Some(&mut deferred_self_fk_checks),
                                )
                                .await
                                .map_err(|batch_err| {
                                    let line_no = batch_err
                                        .failed_row_offset()
                                        .and_then(|offset| line_numbers.get(offset).copied());
                                    let err = batch_err.source_error();
                                    let message = if should_add_copy_insert_context(err) {
                                        if let Some(line_no) = line_no {
                                            format!(
                                                "{}\nCONTEXT:  COPY {}, line {}",
                                                err, short_table, line_no
                                            )
                                        } else {
                                            err.to_string()
                                        }
                                    } else {
                                        err.to_string()
                                    };
                                    PgWireError::UserError(Box::new(ErrorInfo::new(
                                        "ERROR".to_string(),
                                        sqlstate_for_executor_error(err).to_string(),
                                        message,
                                    )))
                                })?;

                            // Deferred self-FK validation: all rows are now in storage
                            // within the transaction. Validate accumulated child FK
                            // checks against the complete PK key set (matching STDIN
                            // path behavior in on_copy_done).
                            if !deferred_self_fk_checks.is_empty() {
                                executor
                                    .validate_copy_deferred_self_fk(
                                        &mut session,
                                        &resolved_table,
                                        &deferred_self_fk_checks,
                                        &pending_self_fk_keys,
                                    )
                                    .await
                                    .map_err(|e| {
                                        PgWireError::UserError(Box::new(ErrorInfo::new(
                                            "ERROR".to_string(),
                                            sqlstate_for_executor_error(&e).to_string(),
                                            e.to_string(),
                                        )))
                                    })?;
                            }

                            Ok(())
                        },
                    ),
                )
                .await;
            insert_result?;

            Ok(row_count)
        }
        .await;

        match result {
            Ok(row_count) => {
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
            Err(e) => {
                rollback_autocommit_or_mark_failed(&mut session, started_txn).await;
                Err(e)
            }
        }
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

        if self.cancel_token.is_cancelled() {
            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                "FATAL".to_string(),
                "25P03".to_string(),
                "terminating connection due to idle-in-transaction timeout".to_string(),
            ))));
        }

        if let Err(e) = session.check_idle_in_transaction_timeout() {
            let _ = session.rollback().await;
            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                "FATAL".to_string(),
                e.sqlstate().to_string(),
                e.to_string(),
            ))));
        }

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

        // All fallible work after begin() is wrapped in an async block so that
        // every `?` is caught by the single cleanup site below.
        let result: PgWireResult<usize> = async {
            // Server-file privilege check first for fs9:// URLs (same as
            // the CSV/TEXT handler — PG checks file permission before table
            // privilege for COPY FROM 'filename').
            if crate::extensions::parquet::reader::is_fs9_url(&url) && !session.is_superuser() {
                return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                    "ERROR".to_string(),
                    "42501".to_string(),
                    "permission denied to COPY from a file".to_string(),
                ))));
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

            // Delegate streaming import to Executor under the same statement
            // runtime context contract as regular statements and COPY STDIN.
            let runtime = crate::sql::runtime_context::StatementRuntimeContext::from_session(
                &session,
                executor.tenant_keyspace(),
                executor.store().transaction_client(),
            );
            let row_count = crate::sql::query_context::with_scoped_query_context(
                &qctx,
                crate::sql::runtime_context::wrap_with_statement_runtime_context(&runtime, async {
                    executor
                        .execute_copy_from_parquet(&mut session, &table_name, &url, started_txn)
                        .await
                }),
            )
            .await
            .map_err(|e| user_error("XX000", e.to_string()))?;

            Ok(row_count)
        }
        .await;

        match result {
            Ok(row_count) => {
                if started_txn {
                    session
                        .commit()
                        .await
                        .map_err(|e| user_error("XX000", e.to_string()))?;
                }
                Ok(Some(vec![pgwire::api::results::Response::Execution(
                    pgwire::api::results::Tag::new("COPY").with_rows(row_count),
                )]))
            }
            Err(e) => {
                rollback_autocommit_or_mark_failed(&mut session, started_txn).await;
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::copy_from_fs9_io_error;

    #[test]
    fn copy_from_fs9_io_error_maps_to_user_error_58030() {
        let err = copy_from_fs9_io_error("backend unavailable");
        match err {
            pgwire::error::PgWireError::UserError(info) => {
                assert_eq!(info.code, "58030");
                assert_eq!(info.severity, "ERROR");
                assert!(info.message.starts_with("COPY FROM fs9: "));
                assert!(info.message.contains("backend unavailable"));
            }
            other => panic!("expected UserError, got {other:?}"),
        }
    }
}
