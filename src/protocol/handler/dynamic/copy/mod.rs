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

use helpers::{copy_display_table_name, parse_copy_text_line, should_add_copy_insert_context};

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

            (
                parse_res,
                table_name,
                started_txn,
                qctx,
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

        let insert_res: PgWireResult<()> =
            crate::sql::query_context::with_scoped_query_context(&qctx, async {
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
                            rows_to_insert,
                            Some(&mut accumulated_fk_keys),
                            Some(&mut accumulated_deferred_fk),
                        )
                        .await
                        .map_err(|batch_err| {
                            let line_no = batch_err
                                .failed_row_offset()
                                .and_then(|offset| line_numbers.get(offset).copied());
                            let err = batch_err.source_error();
                            error!("COPY insert error: {}", err);
                            let table = copy_display_table_name(&table_name);
                            let message = if should_add_copy_insert_context(err) {
                                if let Some(line_no) = line_no {
                                    format!("{}\nCONTEXT:  COPY {}, line {}", err, table, line_no)
                                } else {
                                    err.to_string()
                                }
                            } else {
                                err.to_string()
                            };
                            user_error(sqlstate_for_executor_error(err), message)
                        })?;

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

            crate::sql::query_context::with_scoped_query_context(&qctx, async {
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
