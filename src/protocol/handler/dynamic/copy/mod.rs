//! COPY protocol handling for [`DynamicPgHandler`].
//!
//! Contains `parse_copy_command`, `parse_copy_to_command`, COPY execution
//! helpers, and the [`CopyHandler`] trait implementation.
//!
//! Sub-modules:
//! - `parse`    — COPY FROM STDIN / COPY TO STDOUT command parsing
//! - `helpers`  — shared error/parse helpers
//! - `response` — COPY TO STDOUT response building
//! - `fs9`      — fs9 remote COPY support (feature-gated)

mod export;
mod helpers;
mod parse;
mod response;

#[cfg(feature = "parquet")]
mod fs9;

use super::DynamicPgHandler;
use crate::model::Value;
use async_trait::async_trait;
use futures::{Sink, SinkExt};
use pgwire::api::copy::CopyHandler;
use pgwire::api::ClientInfo;
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::copy::{CopyData, CopyDone, CopyFail};
use pgwire::messages::response::CommandComplete;
use pgwire::messages::PgWireBackendMessage;
use std::fmt::Debug;
use tracing::{error, warn};

use super::super::errors::{
    in_failed_sql_transaction_pgwire_error, sqlstate_for_executor_error, user_error,
};
use super::super::rollback_autocommit_or_mark_failed;

use helpers::{copy_display_table_name, parse_copy_input_line, should_add_copy_insert_context};

async fn with_copy_statement_context<R: Send>(
    qctx: &crate::sql::query_context::QueryContext,
    runtime: &crate::sql::runtime_context::StatementRuntimeContext,
    future: impl std::future::Future<Output = PgWireResult<R>> + Send,
) -> PgWireResult<R> {
    crate::sql::query_context::with_scoped_query_context(
        qctx,
        crate::sql::runtime_context::wrap_with_statement_runtime_context(runtime, future),
    )
    .await
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

        // COPY FROM STDIN runs outside the original simple-query `do_query` scope,
        // so acquire admission here and keep the guard in CopyContext until COPY ends.
        let copy_admission = {
            let mut ctx_guard = self.copy_context.lock().await;
            let Some(ctx) = ctx_guard.as_mut() else {
                return Ok(());
            };

            if ctx.backpressure_guard.is_none() {
                match self.check_backpressure() {
                    Ok(guard) => {
                        ctx.backpressure_guard = guard;
                        Ok(())
                    }
                    Err(e) => {
                        let started_txn = ctx.started_txn;
                        *ctx_guard = None;
                        Err((e, started_txn))
                    }
                }
            } else {
                Ok(())
            }
        };

        if let Err((e, started_txn)) = copy_admission {
            let mut session = self.auth().session.lock().await;
            rollback_autocommit_or_mark_failed(&mut session, started_txn).await;
            return Err(e);
        }

        let (
            parse_res,
            table_name,
            started_txn,
            qctx,
            runtime,
            mut accumulated_fk_keys,
            mut accumulated_deferred_fk,
        ) = {
            let mut ctx_guard = self.copy_context.lock().await;
            let Some(ctx) = ctx_guard.as_mut() else {
                return Ok(());
            };

            let table_name = ctx.table_name.clone();
            let started_txn = ctx.started_txn;
            let qctx = ctx.query_context.clone();
            let runtime = ctx.runtime_context.clone();
            let accumulated_fk_keys = std::mem::take(&mut ctx.pending_self_fk_keys);
            let accumulated_deferred_fk = std::mem::take(&mut ctx.deferred_self_fk_checks);

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

                    // HEADER option: skip the first data line (header row).
                    if ctx.copy_options.header && !ctx.header_skipped {
                        ctx.header_skipped = true;
                        continue;
                    }

                    let line_no = ctx
                        .row_count
                        .saturating_add(rows_to_insert.len())
                        .saturating_add(1);
                    let col_values = parse_copy_input_line(
                        executor,
                        &ctx.table_name,
                        &ctx.columns,
                        &ctx.column_types,
                        line_no,
                        &line_bytes,
                        &ctx.copy_options,
                    )?;
                    rows_to_insert.push((line_no, col_values));
                }

                Ok(rows_to_insert)
            })();

            (
                parse_res,
                table_name,
                started_txn,
                qctx,
                runtime,
                accumulated_fk_keys,
                accumulated_deferred_fk,
            )
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
            // Write back accumulated state even on empty frames to avoid
            // dropping previously accumulated FK keys via mem::take.
            let mut ctx_guard = self.copy_context.lock().await;
            if let Some(ctx) = ctx_guard.as_mut() {
                ctx.pending_self_fk_keys = accumulated_fk_keys;
                ctx.deferred_self_fk_checks = accumulated_deferred_fk;
            }
            return Ok(());
        }
        let line_numbers: Vec<usize> = rows_to_insert.iter().map(|(line_no, _)| *line_no).collect();
        let rows_to_insert: Vec<Vec<(String, Value)>> = rows_to_insert
            .into_iter()
            .map(|(_, col_values)| col_values)
            .collect();

        // Transaction rotation for autocommit COPY FROM STDIN.
        // Dynamically computes chunk sizes based on current deferred FK state
        // after each batch, so rotation resumes immediately when forward
        // self-FK references are resolved.
        use crate::protocol::handler::copy::COPY_STDIN_COMMIT_SIZE;

        let rotation_enabled = started_txn;

        let prev_batch_rows = {
            let ctx_guard = self.copy_context.lock().await;
            ctx_guard
                .as_ref()
                .map_or(0, |ctx| ctx.batch_rows_since_commit)
        };
        let mut batch_rows_since_commit = prev_batch_rows;

        let total_rows = inserted_count;
        let mut offset: usize = 0;

        while offset < rows_to_insert.len() {
            // Compute next chunk size dynamically based on remaining capacity.
            let remaining_capacity = if rotation_enabled {
                COPY_STDIN_COMMIT_SIZE
                    .saturating_sub(batch_rows_since_commit)
                    .max(1) // at least 1 row per chunk to make progress
            } else {
                rows_to_insert.len() - offset // all remaining in one chunk
            };
            let end = (offset + remaining_capacity).min(rows_to_insert.len());
            let chunk_line_numbers = &line_numbers[offset..end];
            let chunk_rows = rows_to_insert[offset..end].to_vec();
            let chunk_len = chunk_rows.len();

            let insert_res: PgWireResult<()> =
                with_copy_statement_context(&qctx, &runtime, async {
                    let mut session = self.auth().session.lock().await;

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

                    let savepoints = session.savepoints();
                    crate::txn::with_savepoints(savepoints, async {
                        executor
                            .execute_copy_insert_batch(
                                &mut session,
                                &table_name,
                                chunk_rows,
                                Some(&mut accumulated_fk_keys),
                                Some(&mut accumulated_deferred_fk),
                            )
                            .await
                            .map_err(|batch_err| {
                                let line_no = batch_err
                                    .failed_row_offset()
                                    .and_then(|o| chunk_line_numbers.get(o).copied());
                                let err = batch_err.source_error();
                                error!("COPY insert error: {}", err);
                                let table = copy_display_table_name(&table_name);
                                let message = if should_add_copy_insert_context(err) {
                                    if let Some(line_no) = line_no {
                                        format!(
                                            "{}\nCONTEXT:  COPY {}, line {}",
                                            err, table, line_no
                                        )
                                    } else {
                                        err.to_string()
                                    }
                                } else {
                                    err.to_string()
                                };
                                user_error(sqlstate_for_executor_error(err), message)
                            })?;

                        // After each batch: prune resolved deferred self-FK checks,
                        // then decide whether to commit + rotate.
                        batch_rows_since_commit += chunk_len;
                        if !accumulated_deferred_fk.is_empty() {
                            accumulated_deferred_fk.retain(|(_, fk_name, hash_key, _)| {
                                !accumulated_fk_keys
                                    .get(fk_name)
                                    .is_some_and(|keys| keys.contains(hash_key))
                            });
                        }
                        // Hard ceiling: fail fast if rotation is blocked and the
                        // transaction has grown too large. This prevents the silent
                        // growth to TiKV's 100MB limit that #1988 was filed for.
                        if rotation_enabled
                            && !accumulated_deferred_fk.is_empty()
                            && batch_rows_since_commit
                                >= crate::protocol::handler::copy::COPY_STDIN_MAX_UNROTATED_ROWS
                        {
                            return Err(user_error(
                                "54000",
                                format!(
                                    "COPY exceeds transaction size limit ({} rows). \
                                     Table \"{}\" has self-referencing foreign keys with \
                                     forward references (child rows before parent rows) \
                                     that prevent transaction rotation. Reorder rows so \
                                     parent rows appear before child rows, or split the \
                                     import into smaller batches.",
                                    batch_rows_since_commit,
                                    copy_display_table_name(&table_name),
                                ),
                            ));
                        }
                        if rotation_enabled
                            && accumulated_deferred_fk.is_empty()
                            && batch_rows_since_commit >= COPY_STDIN_COMMIT_SIZE
                        {
                            session.commit().await.map_err(|e| {
                                user_error(
                                    "XX000",
                                    format!("COPY transaction rotation commit failed: {}", e),
                                )
                            })?;
                            session.begin().await.map_err(|e| {
                                user_error(
                                    "XX000",
                                    format!("COPY transaction rotation begin failed: {}", e),
                                )
                            })?;
                            batch_rows_since_commit = 0;
                            tracing::info!(
                                "COPY FROM STDIN: {} rows committed (rotation)",
                                offset + chunk_len,
                            );
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

            offset = end;
        }

        let mut ctx_guard = self.copy_context.lock().await;
        if let Some(ctx) = ctx_guard.as_mut() {
            ctx.row_count = ctx.row_count.saturating_add(total_rows);
            ctx.batch_rows_since_commit = batch_rows_since_commit;
            ctx.pending_self_fk_keys = accumulated_fk_keys;
            ctx.deferred_self_fk_checks = accumulated_deferred_fk;
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
            let runtime = ctx.runtime_context.clone();

            with_copy_statement_context(&qctx, &runtime, async {
                let mut session = self.auth().session.lock().await;

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

                if let Some(final_line_bytes) = ctx.drain_final_line() {
                    if final_line_bytes.as_slice() == b"\\." {
                        ctx.reached_end_marker = true;
                    } else if ctx.copy_options.header && !ctx.header_skipped {
                        // Header row arrived without trailing newline — skip it.
                        ctx.header_skipped = true;
                    } else if !ctx.reached_end_marker {
                        let line_no = ctx.row_count.saturating_add(1);
                        let col_values = match parse_copy_input_line(
                            executor,
                            &ctx.table_name,
                            &ctx.columns,
                            &ctx.column_types,
                            line_no,
                            &final_line_bytes,
                            &ctx.copy_options,
                        ) {
                            Ok(values) => values,
                            Err(e) => {
                                rollback_autocommit_or_mark_failed(&mut session, ctx.started_txn)
                                    .await;
                                return Err(e);
                            }
                        };

                        let savepoints = session.savepoints();
                        let line_numbers = [line_no];
                        let mut final_fk_keys = std::mem::take(&mut ctx.pending_self_fk_keys);
                        let mut final_deferred = std::mem::take(&mut ctx.deferred_self_fk_checks);
                        let insert_res = crate::txn::with_savepoints(savepoints, async {
                            executor
                                .execute_copy_insert_batch(
                                    &mut session,
                                    &ctx.table_name,
                                    vec![col_values],
                                    Some(&mut final_fk_keys),
                                    Some(&mut final_deferred),
                                )
                                .await
                                .map_err(|batch_err| {
                                    let line_no = batch_err
                                        .failed_row_offset()
                                        .and_then(|offset| line_numbers.get(offset).copied());
                                    let err = batch_err.source_error();
                                    error!("COPY insert error: {}", err);
                                    let table = copy_display_table_name(&ctx.table_name);
                                    let message = if should_add_copy_insert_context(err) {
                                        if let Some(line_no) = line_no {
                                            format!(
                                                "{}\nCONTEXT:  COPY {}, line {}",
                                                err, table, line_no
                                            )
                                        } else {
                                            err.to_string()
                                        }
                                    } else {
                                        err.to_string()
                                    };
                                    user_error(sqlstate_for_executor_error(err), message)
                                })
                        })
                        .await;

                        if let Err(e) = insert_res {
                            rollback_autocommit_or_mark_failed(&mut session, ctx.started_txn).await;
                            return Err(e);
                        }

                        ctx.pending_self_fk_keys = final_fk_keys;
                        ctx.deferred_self_fk_checks = final_deferred;
                        ctx.row_count = ctx.row_count.saturating_add(1);
                    }
                }

                // Deferred self-FK validation: all rows from every CopyData
                // chunk are now in storage (within the transaction).  Validate
                // unresolved child FK references against the complete PK key
                // set accumulated during COPY.
                if !ctx.deferred_self_fk_checks.is_empty() {
                    if let Err(e) = executor
                        .validate_copy_deferred_self_fk(
                            &mut session,
                            &ctx.table_name,
                            &ctx.deferred_self_fk_checks,
                            &ctx.pending_self_fk_keys,
                        )
                        .await
                    {
                        error!("COPY deferred self-FK error: {}", e);
                        let err_msg = e.to_string();
                        let sqlstate = sqlstate_for_executor_error(&e);
                        rollback_autocommit_or_mark_failed(&mut session, ctx.started_txn).await;
                        return Err(user_error(sqlstate, err_msg));
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
        {
            let mut session = self.auth().session.lock().await;

            if self.cancel_token.is_cancelled() {
                return PgWireError::UserError(Box::new(ErrorInfo::new(
                    "FATAL".to_string(),
                    "25P03".to_string(),
                    "terminating connection due to idle-in-transaction timeout".to_string(),
                )));
            }

            if let Err(e) = session.check_idle_in_transaction_timeout() {
                let _ = session.rollback().await;
                return PgWireError::UserError(Box::new(ErrorInfo::new(
                    "FATAL".to_string(),
                    e.sqlstate().to_string(),
                    e.to_string(),
                )));
            }
        }

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

#[cfg(test)]
mod tests {
    use super::with_copy_statement_context;
    use crate::sql::query_context::QueryContext;
    use crate::sql::runtime_context::{RuntimeSettings, StatementRuntimeContext};
    use pgwire::error::PgWireResult;
    use std::collections::HashSet;
    use std::sync::Arc;

    #[tokio::test]
    async fn copy_statement_context_provides_full_statement_runtime_context() {
        let qctx = QueryContext::for_tests();
        let runtime = StatementRuntimeContext {
            settings: RuntimeSettings {
                is_superuser: false,
                bypass_rls: false,
                timezone: Arc::from("UTC"),
                max_sort_bytes: 4096,
                hash_join_work_mem: 256 * 1024 * 1024,
                search_path: Arc::new(vec!["public".to_string()]),
                text_search_config: Arc::from("simple"),
            },
            tenant_keyspace: Arc::from("copy_tenant"),
            database_id: 88,
            is_in_transaction: false,
            caller_sub: None,
            txn_snapshot_ts_version: Some(1234),
            session_txn_tracker: None,
            tikv_client: None,
            extension_txn_delta: Arc::new((
                HashSet::from(["embedding".to_string()]),
                HashSet::new(),
            )),
            extension_statement_state: Arc::new(
                crate::extensions::context::ExtensionStatementState::default(),
            ),
        };

        let result: PgWireResult<()> = with_copy_statement_context(&qctx, &runtime, async {
            assert_eq!(
                crate::extensions::context::tenant_keyspace().as_deref(),
                Some("copy_tenant")
            );
            assert!(!crate::extensions::context::is_superuser());
            assert_eq!(crate::session_context::current_database_id(), 88);
            assert_eq!(
                crate::session_context::current_txn_snapshot_ts_version(),
                Some(1234)
            );
            assert_eq!(
                crate::session_context::extension_txn_status("embedding"),
                Some(true)
            );
            crate::extensions::context::try_consume_embedding_call()
                .expect("copy statement should have embedding context");
            Ok(())
        })
        .await;

        result.expect("copy statement context should succeed");
    }

    #[tokio::test]
    async fn copy_statement_context_reuses_statement_extension_state_across_reentry() {
        let qctx = QueryContext::for_tests();
        let runtime = StatementRuntimeContext {
            settings: RuntimeSettings {
                is_superuser: false,
                bypass_rls: false,
                timezone: Arc::from("UTC"),
                max_sort_bytes: 4096,
                hash_join_work_mem: 256 * 1024 * 1024,
                search_path: Arc::new(vec!["public".to_string()]),
                text_search_config: Arc::from("simple"),
            },
            tenant_keyspace: Arc::from("copy_tenant"),
            database_id: 88,
            is_in_transaction: false,
            caller_sub: None,
            txn_snapshot_ts_version: Some(1234),
            session_txn_tracker: None,
            tikv_client: None,
            extension_txn_delta: Arc::new((HashSet::new(), HashSet::new())),
            extension_statement_state: Arc::new(
                crate::extensions::context::ExtensionStatementState::default(),
            ),
        };

        let first: PgWireResult<()> = with_copy_statement_context(&qctx, &runtime, async {
            crate::extensions::context::try_consume_embedding_call_with_limit(1)
                .expect("first re-entry should consume the shared statement budget");
            Ok(())
        })
        .await;
        first.expect("first copy statement context should succeed");

        let second: PgWireResult<()> = with_copy_statement_context(&qctx, &runtime, async {
            crate::extensions::context::try_consume_embedding_call_with_limit(1)
                .expect_err("second re-entry must see the same shared statement state");
            Ok(())
        })
        .await;
        second.expect("second copy statement context should succeed");
    }
}
