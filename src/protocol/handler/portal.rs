use futures::{Sink, SinkExt, StreamExt};
use pgwire::api::portal::Portal;
use pgwire::api::query::{ExtendedQueryHandler, SimpleQueryHandler};
use pgwire::api::results::{QueryResponse, Response, Tag};
use pgwire::api::store::PortalStore;
use pgwire::api::{ClientInfo, ClientPortalStore, PgWireConnectionState};
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::data::{DataRow, FieldDescription, RowDescription};
use pgwire::messages::response::{EmptyQueryResponse, TransactionStatus};
use pgwire::messages::PgWireBackendMessage;
use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::fmt::Debug;
use tokio::sync::Mutex;

use super::datatype_to_pgtype;
use super::prepared::{PreparedExec, PreparedStatement};
use crate::pool::{
    run_with_statement_memory_scope, split_statement_memory_scope, try_grow_statement_memory_scope,
    TenantMemoryAccountant, TenantMemoryReservation,
};

fn is_empty_simple_query(query: &str) -> bool {
    let trimmed = query.trim();
    trimmed.is_empty() || trimmed == ";"
}

pub(in crate::protocol::handler) fn update_tx_status_after_execution(
    status: TransactionStatus,
    tag: &Tag,
) -> TransactionStatus {
    if *tag == Tag::new("ROLLBACK") {
        // `ROLLBACK TO SAVEPOINT` clears the failed-transaction state without ending the
        // transaction block, so ReadyForQuery must move from `E` -> `T`.
        TransactionStatus::Transaction
    } else {
        status
    }
}

pub(in crate::protocol::handler) async fn on_query_with_tx_status_fix<H, C>(
    handler: &H,
    statement_memory_accountant: Option<TenantMemoryAccountant>,
    client: &mut C,
    query: pgwire::messages::simplequery::Query,
) -> PgWireResult<()>
where
    H: SimpleQueryHandler,
    C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
    C::Error: Debug,
    PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
{
    run_with_statement_memory_scope(statement_memory_accountant, async {
        if !matches!(client.state(), PgWireConnectionState::ReadyForQuery) {
            return Err(PgWireError::NotReadyForQuery);
        }

        let mut transaction_status = client.transaction_status();
        client.set_state(PgWireConnectionState::QueryInProgress);
        let query_string = query.query;

        if is_empty_simple_query(&query_string) {
            client
                .feed(PgWireBackendMessage::EmptyQueryResponse(
                    EmptyQueryResponse::new(),
                ))
                .await?;
        } else {
            // Memory reservations created during execution/response construction must
            // remain live through wire send. Keeping statement scope wrapped around
            // this send loop guarantees that contract.
            let resp = <H as SimpleQueryHandler>::do_query(handler, client, &query_string).await?;
            for r in resp {
                match r {
                    Response::EmptyQuery => {
                        client
                            .feed(PgWireBackendMessage::EmptyQueryResponse(
                                EmptyQueryResponse::new(),
                            ))
                            .await?;
                    }
                    Response::Query(results) => {
                        pgwire::api::query::send_query_response(client, results, true).await?;
                    }
                    Response::Execution(tag) => {
                        transaction_status =
                            update_tx_status_after_execution(transaction_status, &tag);
                        pgwire::api::query::send_execution_response(client, tag).await?;
                    }
                    Response::TransactionStart(tag) => {
                        pgwire::api::query::send_execution_response(client, tag).await?;
                        transaction_status = transaction_status.to_in_transaction_state();
                    }
                    Response::TransactionEnd(tag) => {
                        pgwire::api::query::send_execution_response(client, tag).await?;
                        transaction_status = transaction_status.to_idle_state();
                    }
                    Response::Error(e) => {
                        client
                            .feed(PgWireBackendMessage::ErrorResponse((*e).into()))
                            .await?;
                        transaction_status = transaction_status.to_error_state();
                    }
                    Response::CopyIn(result) => {
                        pgwire::api::copy::send_copy_in_response(client, result).await?;
                        client.set_state(PgWireConnectionState::CopyInProgress(false));
                    }
                    Response::CopyOut(result) => {
                        pgwire::api::copy::send_copy_out_response(client, result).await?;
                        client.set_state(PgWireConnectionState::CopyInProgress(false));
                    }
                    Response::CopyBoth(result) => {
                        pgwire::api::copy::send_copy_both_response(client, result).await?;
                        client.set_state(PgWireConnectionState::CopyInProgress(false));
                    }
                }
            }
        }

        if !matches!(client.state(), PgWireConnectionState::CopyInProgress(_)) {
            client.set_state(PgWireConnectionState::ReadyForQuery);
            client.set_transaction_status(transaction_status);
            pgwire::api::query::send_ready_for_query(client, transaction_status).await?;
        }

        Ok(())
    })
    .await
}

pub(in crate::protocol::handler) async fn on_execute_with_tx_status_fix<H, C>(
    handler: &H,
    suspended_portals: &Mutex<HashMap<String, SuspendedPortalState>>,
    statement_memory_accountant: Option<TenantMemoryAccountant>,
    client: &mut C,
    message: pgwire::messages::extendedquery::Execute,
) -> PgWireResult<()>
where
    H: ExtendedQueryHandler,
    H::Statement: 'static,
    C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
    C::PortalStore: PortalStore<Statement = H::Statement>,
    C::Error: Debug,
    PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
{
    run_with_statement_memory_scope(statement_memory_accountant, async {
        if !matches!(client.state(), PgWireConnectionState::ReadyForQuery) {
            return Err(PgWireError::NotReadyForQuery);
        }
        let mut transaction_status = client.transaction_status();

        client.set_state(PgWireConnectionState::QueryInProgress);

        let portal_name = message.name.as_deref().unwrap_or(pgwire::api::DEFAULT_NAME);
        let max_rows = if message.max_rows <= 0 {
            0
        } else {
            message.max_rows as usize
        };

        if let Some(portal) = client.portal_store().get_portal(portal_name) {
            if let Some((
                command_tag,
                chunk,
                still_suspended,
                total_rows_sent,
                drained_reservation,
            )) = take_suspended_rows(suspended_portals, portal_name, max_rows).await
            {
                // Hold drained reservation through wire send for this Execute.
                let _drained_reservation = drained_reservation;
                for row in chunk {
                    client.feed(PgWireBackendMessage::DataRow(row)).await?;
                }

                if still_suspended {
                    client
                        .send(PgWireBackendMessage::PortalSuspended(
                            pgwire::messages::extendedquery::PortalSuspended::new(),
                        ))
                        .await?;
                } else {
                    let tag = Tag::new(&command_tag).with_rows(total_rows_sent);
                    client
                        .send(PgWireBackendMessage::CommandComplete(tag.into()))
                        .await?;
                }
            } else {
                // Scope wraps both execution and downstream send so reservations
                // remain alive until wire emission completes.
                match <H as ExtendedQueryHandler>::do_query(
                    handler,
                    client,
                    portal.as_ref(),
                    max_rows,
                )
                .await?
                {
                    Response::EmptyQuery => {
                        client
                            .feed(PgWireBackendMessage::EmptyQueryResponse(
                                EmptyQueryResponse::new(),
                            ))
                            .await?;
                    }
                    Response::Query(results) => {
                        let send_describe =
                            should_send_row_description_for_portal(portal.as_ref(), &results);
                        if max_rows == 0 {
                            pgwire::api::query::send_query_response(client, results, send_describe)
                                .await?;
                        } else {
                            send_limited_query_response(
                                client,
                                suspended_portals,
                                portal_name,
                                results,
                                max_rows,
                                send_describe,
                            )
                            .await?;
                        }
                    }
                    Response::Execution(tag) => {
                        transaction_status =
                            update_tx_status_after_execution(transaction_status, &tag);
                        pgwire::api::query::send_execution_response(client, tag).await?;
                    }
                    Response::TransactionStart(tag) => {
                        pgwire::api::query::send_execution_response(client, tag).await?;
                        transaction_status = transaction_status.to_in_transaction_state();
                    }
                    Response::TransactionEnd(tag) => {
                        pgwire::api::query::send_execution_response(client, tag).await?;
                        transaction_status = transaction_status.to_idle_state();
                    }
                    Response::Error(err) => {
                        client
                            .send(PgWireBackendMessage::ErrorResponse((*err).into()))
                            .await?;
                        transaction_status = transaction_status.to_error_state();
                    }
                    Response::CopyIn(result) => {
                        client.set_state(PgWireConnectionState::CopyInProgress(true));
                        pgwire::api::copy::send_copy_in_response(client, result).await?;
                    }
                    Response::CopyOut(result) => {
                        client.set_state(PgWireConnectionState::CopyInProgress(true));
                        pgwire::api::copy::send_copy_out_response(client, result).await?;
                    }
                    Response::CopyBoth(result) => {
                        client.set_state(PgWireConnectionState::CopyInProgress(true));
                        pgwire::api::copy::send_copy_both_response(client, result).await?;
                    }
                }
            }

            if !matches!(client.state(), PgWireConnectionState::CopyInProgress(_)) {
                client.set_state(PgWireConnectionState::ReadyForQuery);
                client.set_transaction_status(transaction_status);
            };

            Ok(())
        } else {
            Err(PgWireError::PortalNotFound(portal_name.to_owned()))
        }
    })
    .await
}

#[derive(Debug)]
pub(in crate::protocol::handler) struct SuspendedPortalState {
    command_tag: String,
    remaining_rows: VecDeque<DataRow>,
    /// Ownership handoff target for suspended remainder bytes.
    /// Created at suspend-buffer creation and released by:
    /// - drain (split to current execute scope),
    /// - explicit state removal (Close/rebind),
    /// - final state drop when remainder becomes empty.
    buffer_reservation: Option<TenantMemoryReservation>,
    rows_sent_so_far: usize,
}

const DEFAULT_MAX_SUSPENDED_PORTALS: usize = 32;
const DEFAULT_MAX_SUSPENDED_PORTAL_BUFFER_ROWS: usize = 10_000;
const DEFAULT_MAX_SUSPENDED_PORTAL_BUFFER_BYTES: usize = 16 * 1024 * 1024;

pub(in crate::protocol::handler) fn max_suspended_portals() -> usize {
    std::env::var("DB9_MAX_SUSPENDED_PORTALS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_MAX_SUSPENDED_PORTALS)
}

pub(in crate::protocol::handler) fn max_suspended_portal_buffer_rows() -> usize {
    std::env::var("DB9_MAX_SUSPENDED_PORTAL_BUFFER_ROWS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_MAX_SUSPENDED_PORTAL_BUFFER_ROWS)
}

fn max_suspended_portal_buffer_bytes() -> usize {
    std::env::var("DB9_MAX_SUSPENDED_PORTAL_BUFFER_BYTES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_MAX_SUSPENDED_PORTAL_BUFFER_BYTES)
}

async fn take_suspended_rows(
    suspended_portals: &Mutex<HashMap<String, SuspendedPortalState>>,
    portal_name: &str,
    max_rows: usize,
) -> Option<(
    String,
    Vec<DataRow>,
    bool,
    usize,
    Option<TenantMemoryReservation>,
)> {
    let mut guard = suspended_portals.lock().await;
    let state = guard.get_mut(portal_name)?;

    let to_take = if max_rows == 0 {
        state.remaining_rows.len()
    } else {
        max_rows.min(state.remaining_rows.len())
    };

    let mut chunk = Vec::with_capacity(to_take);
    let mut drained_bytes = 0usize;
    for _ in 0..to_take {
        if let Some(row) = state.remaining_rows.pop_front() {
            drained_bytes = drained_bytes.saturating_add(row.data.len());
            chunk.push(row);
        }
    }

    state.rows_sent_so_far = state.rows_sent_so_far.saturating_add(chunk.len());
    let still_suspended = !state.remaining_rows.is_empty();
    let command_tag = state.command_tag.clone();
    let total_rows_sent = state.rows_sent_so_far;
    // Explicit drain ownership transfer:
    // move drained bytes from portal-owned reservation to this Execute's
    // temporary owner (kept alive in caller through wire send).
    let drained_reservation = state
        .buffer_reservation
        .as_mut()
        .and_then(|r| r.split(drained_bytes));

    if !still_suspended {
        guard.remove(portal_name);
    }

    Some((
        command_tag,
        chunk,
        still_suspended,
        total_rows_sent,
        drained_reservation,
    ))
}

async fn send_limited_query_response<C>(
    client: &mut C,
    suspended_portals: &Mutex<HashMap<String, SuspendedPortalState>>,
    portal_name: &str,
    results: QueryResponse<'_>,
    max_rows: usize,
    send_describe: bool,
) -> PgWireResult<()>
where
    C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
    C::Error: Debug,
    PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
{
    let command_tag = results.command_tag().to_owned();
    if send_describe {
        let row_schema = results.row_schema();
        let row_desc = RowDescription::new(
            row_schema
                .iter()
                .map(FieldDescription::from)
                .collect::<Vec<_>>(),
        );
        client
            .send(PgWireBackendMessage::RowDescription(row_desc))
            .await?;
    }
    let mut data_rows = results.data_rows();

    let mut rows_sent = 0usize;
    let mut remainder: VecDeque<DataRow> = VecDeque::new();
    let mut buffered_bytes: usize = 0;
    let max_buffered_rows = max_suspended_portal_buffer_rows();
    let max_buffered_bytes = max_suspended_portal_buffer_bytes();

    while let Some(row) = data_rows.next().await {
        let row = row?;
        if rows_sent < max_rows {
            rows_sent += 1;
            client.feed(PgWireBackendMessage::DataRow(row)).await?;
        } else {
            // Statement-scope charge while buffering remainder.
            if let Err(e) =
                try_grow_statement_memory_scope("protocol.portal.suspend_buffer", row.data.len())
            {
                return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                    "ERROR".to_owned(),
                    e.sqlstate().to_owned(),
                    e.to_string(),
                ))));
            }
            buffered_bytes = buffered_bytes.saturating_add(row.data.len());
            remainder.push_back(row);
            if remainder.len() > max_buffered_rows || buffered_bytes > max_buffered_bytes {
                return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                    "ERROR".to_owned(),
                    "54000".to_owned(),
                    format!(
                        "portal suspension buffer exceeded (portal={portal_name}, max_rows={max_rows}, buffer_rows_limit={max_buffered_rows}, buffer_bytes_limit={max_buffered_bytes}); re-run with max_rows=0 or reduce result size; set DB9_MAX_SUSPENDED_PORTAL_BUFFER_ROWS/BYTES to override"
                    ),
                ))));
            }
        }
    }

    if !remainder.is_empty() {
        let max_suspended = max_suspended_portals();
        let mut guard = suspended_portals.lock().await;
        if !guard.contains_key(portal_name) && guard.len() >= max_suspended {
            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                "ERROR".to_owned(),
                "54000".to_owned(),
                format!(
                    "too many suspended portals (portal={portal_name}, max_rows={max_rows}, suspended_portals_limit={max_suspended}); close portals to free resources or re-run with max_rows=0; set DB9_MAX_SUSPENDED_PORTALS to override"
                ),
            ))));
        }
        // Ownership handoff point: split buffered bytes out of the current
        // statement scope and attach to suspended portal state.
        let buffer_reservation = split_statement_memory_scope(buffered_bytes);
        guard.insert(
            portal_name.to_owned(),
            SuspendedPortalState {
                command_tag,
                remaining_rows: remainder,
                buffer_reservation,
                rows_sent_so_far: rows_sent,
            },
        );
        client
            .send(PgWireBackendMessage::PortalSuspended(
                pgwire::messages::extendedquery::PortalSuspended::new(),
            ))
            .await?;
    } else {
        let tag = Tag::new(&command_tag).with_rows(rows_sent);
        client
            .send(PgWireBackendMessage::CommandComplete(tag.into()))
            .await?;
    }

    Ok(())
}

fn should_send_row_description_for_portal<S: 'static>(
    portal: &Portal<S>,
    results: &QueryResponse<'_>,
) -> bool {
    let stmt_any = &portal.statement.statement as &dyn Any;
    let Some(prepared) = stmt_any.downcast_ref::<PreparedStatement>() else {
        return false;
    };

    // Utility prepared statements use static Describe metadata and do not
    // participate in schema-drift fallback.
    if matches!(prepared.exec, PreparedExec::RawSqlUtility) {
        return false;
    }

    let runtime_schema = results.row_schema();
    if prepared.output_schema.len() != runtime_schema.len() {
        return true;
    }

    for ((expected_name, expected_dt), actual) in
        prepared.output_schema.iter().zip(runtime_schema.iter())
    {
        if expected_name != actual.name()
            || datatype_to_pgtype(Some(expected_dt)) != *actual.datatype()
        {
            return true;
        }
    }

    false
}
