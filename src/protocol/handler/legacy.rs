use super::copy::copy_row_column_mismatch_error;
use super::encode::result_to_response;
use super::errors::{in_failed_sql_transaction_pgwire_error, sqlstate_for_executor_error};
use super::params::{
    count_sql_parameters, dummy_sql_expr_for_param_type, infer_parameter_types,
    substitute_parameters, substitute_placeholders_outside_strings_and_dollar,
};
use super::portal::{
    on_execute_with_tx_status_fix, on_query_with_tx_status_fix, SuspendedPortalState,
};
use super::query_parser::strip_leading_whitespace_and_comments;
use super::{
    client_allows_notice, infer_result_fields_from_query_ast, resolve_copy_columns,
    rollback_autocommit_or_mark_failed, send_notices_and_get_last_response, CopyContext,
    PgServerParameterProvider, TipgQueryParser, CONNECTION_ID_COUNTER,
};
use crate::sql::expr::set_connection_id;
use crate::sql::{ExecuteResult, Executor, Session};
use crate::types::{TableSchema, Value};
use async_trait::async_trait;
use futures::{Sink, SinkExt};
use pgwire::api::auth::StartupHandler;
use pgwire::api::copy::CopyHandler;
use pgwire::api::portal::Portal;
use pgwire::api::query::{ExtendedQueryHandler, SimpleQueryHandler};
use pgwire::api::results::{
    CopyResponse, DescribePortalResponse, DescribeStatementResponse, FieldInfo, Response,
};
use pgwire::api::stmt::StoredStatement;
use pgwire::api::store::PortalStore;
use pgwire::api::{ClientInfo, ClientPortalStore, NoopErrorHandler, PgWireServerHandlers, Type};
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::copy::{CopyData, CopyDone, CopyFail};
use pgwire::messages::response::{CommandComplete, NoticeResponse};
use pgwire::messages::{PgWireBackendMessage, PgWireFrontendMessage};
use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, error, warn};

// Keep the old HandlerFactory for backward compatibility (static executor)
#[allow(dead_code)]
pub struct HandlerFactory {
    handler: Arc<PgHandler>,
}

impl HandlerFactory {
    #[allow(dead_code)]
    pub fn new(executor: Arc<Executor>) -> Self {
        Self {
            handler: Arc::new(PgHandler::new(executor)),
        }
    }
}

#[allow(dead_code)]
pub struct PgHandler {
    executor: Arc<Executor>,
    session: Mutex<Session>,
    copy_context: Mutex<Option<CopyContext>>,
    suspended_portals: Mutex<HashMap<String, SuspendedPortalState>>,
    query_parser: Arc<TipgQueryParser>,
    connection_id: i32,
}

impl PgHandler {
    #[allow(dead_code)]
    pub fn new(executor: Arc<Executor>) -> Self {
        let store = executor.store();
        let observability = executor.observability().clone();
        let connection_id = CONNECTION_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
        Self {
            executor,
            session: Mutex::new(Session::new_with_database(
                store,
                observability,
                connection_id,
                1,
                "postgres".to_string(),
            )),
            copy_context: Mutex::new(None),
            suspended_portals: Mutex::new(HashMap::new()),
            query_parser: Arc::new(TipgQueryParser::new()),
            connection_id,
        }
    }

    #[allow(dead_code)]
    async fn infer_result_fields_from_query(&self, query: &str) -> Vec<FieldInfo> {
        let mut session_guard = self.session.lock().await;
        let store = self.executor.store();
        infer_result_fields_from_query_ast(&store, &mut session_guard, query).await
    }

    #[allow(dead_code)]
    fn parse_copy_command(query: &str) -> Option<(String, Vec<String>)> {
        let query = strip_leading_whitespace_and_comments(query)?;
        let query_upper = query.to_uppercase();
        if !query_upper.starts_with("COPY")
            || !query_upper.contains("FROM")
            || !query_upper.contains("STDIN")
        {
            return None;
        }

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
}

#[async_trait]
impl StartupHandler for PgHandler {
    async fn on_startup<C>(
        &self,
        client: &mut C,
        message: PgWireFrontendMessage,
    ) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        if let PgWireFrontendMessage::Startup(ref startup) = message {
            pgwire::api::auth::save_startup_parameters_to_metadata(client, startup);
            pgwire::api::auth::finish_authentication(client, &PgServerParameterProvider).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl SimpleQueryHandler for PgHandler {
    async fn on_query<C>(
        &self,
        client: &mut C,
        query: pgwire::messages::simplequery::Query,
    ) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        on_query_with_tx_status_fix(self, client, query).await
    }

    async fn do_query<'a, C>(
        &self,
        client: &mut C,
        query: &'a str,
    ) -> PgWireResult<Vec<Response<'a>>>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        debug!("Received query: {}", query);

        if let Some((table_name, columns)) = Self::parse_copy_command(query) {
            debug!(
                "COPY command detected: table={}, columns={:?}",
                table_name, columns
            );

            let (resolved_table, resolved_columns, column_types, col_count, started_txn) = {
                let mut session = self.session.lock().await;
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

                let db_id = session.current_database_id();
                let search_path: Vec<String> = session.search_path().to_vec();
                let (resolved_table, schema) = {
                    let txn = session.get_mut_txn().ok_or_else(|| {
                        PgWireError::UserError(Box::new(ErrorInfo::new(
                            "ERROR".to_string(),
                            "XX000".to_string(),
                            "No transaction".to_string(),
                        )))
                    })?;

                    let strip_quotes = |s: &str| -> String { s.trim_matches('"').to_string() };
                    let (schema_opt, table_ident) = if table_name.contains('.') {
                        let parts: Vec<&str> = table_name.splitn(2, '.').collect();
                        if parts.len() == 2 {
                            (
                                Some(strip_quotes(parts[0]).to_lowercase()),
                                strip_quotes(parts[1]).to_lowercase(),
                            )
                        } else {
                            (None, strip_quotes(&table_name).to_lowercase())
                        }
                    } else {
                        (None, strip_quotes(&table_name).to_lowercase())
                    };

                    if let Some(schema_ident) = schema_opt {
                        let resolved_table = format!("{}.{}", schema_ident, table_ident);
                        match self
                            .executor
                            .store()
                            .get_schema(txn, db_id, &resolved_table)
                            .await
                        {
                            Ok(Some(schema)) => (resolved_table, schema),
                            Ok(None) => {
                                rollback_autocommit_or_mark_failed(&mut session, started_txn).await;
                                return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                                    "ERROR".to_string(),
                                    "42P01".to_string(),
                                    format!("relation \"{}\" does not exist", table_name),
                                ))));
                            }
                            Err(e) => {
                                rollback_autocommit_or_mark_failed(&mut session, started_txn).await;
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

                        let mut found: Option<(String, TableSchema)> = None;
                        for schema_ident in schemas {
                            let resolved_table = format!("{}.{}", schema_ident, table_ident);
                            match self
                                .executor
                                .store()
                                .get_schema(txn, db_id, &resolved_table)
                                .await
                            {
                                Ok(Some(schema)) => {
                                    found = Some((resolved_table, schema));
                                    break;
                                }
                                Ok(None) => continue,
                                Err(e) => {
                                    rollback_autocommit_or_mark_failed(&mut session, started_txn)
                                        .await;
                                    return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                                        "ERROR".to_string(),
                                        "XX000".to_string(),
                                        e.to_string(),
                                    ))));
                                }
                            }
                        }

                        match found {
                            Some(found) => found,
                            None => {
                                rollback_autocommit_or_mark_failed(&mut session, started_txn).await;
                                return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                                    "ERROR".to_string(),
                                    "42P01".to_string(),
                                    format!("relation \"{}\" does not exist", table_name),
                                ))));
                            }
                        }
                    }
                };

                let (resolved_columns, column_types) =
                    match resolve_copy_columns(&schema, &columns, &table_name) {
                        Ok(resolved) => resolved,
                        Err(e) => {
                            rollback_autocommit_or_mark_failed(&mut session, started_txn).await;
                            return Err(e);
                        }
                    };

                let col_count = resolved_columns.len();
                (
                    resolved_table,
                    resolved_columns,
                    column_types,
                    col_count,
                    started_txn,
                )
            };

            let mut ctx = self.copy_context.lock().await;
            *ctx = Some(CopyContext {
                table_name: resolved_table,
                columns: resolved_columns,
                column_types,
                line_buffer: Vec::new(),
                row_count: 0,
                started_txn,
                reached_end_marker: false,
            });

            let column_formats: Vec<i16> = vec![0; col_count];
            return Ok(vec![Response::CopyIn(CopyResponse::new(
                0,
                col_count,
                column_formats,
            ))]);
        }

        let mut session = self.session.lock().await;

        set_connection_id(session.connection_id());

        match self.executor.execute(&mut session, query).await {
            Ok(results) => {
                let mut responses: Vec<Response<'a>> = Vec::new();
                for result in results.into_vec() {
                    if let ExecuteResult::Notice { message } = result {
                        if client_allows_notice(session.show_setting_value("client_min_messages")) {
                            let notice = NoticeResponse::from(ErrorInfo::new(
                                "NOTICE".to_string(),
                                "00000".to_string(),
                                message,
                            ));
                            client
                                .send(PgWireBackendMessage::NoticeResponse(notice))
                                .await?;
                        }
                        continue;
                    }
                    responses.push(result_to_response(result)?);
                }
                Ok(responses)
            }
            Err(e) => {
                error!("Query execution error: {}", e);
                Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                    "ERROR".to_string(),
                    sqlstate_for_executor_error(&e).to_string(),
                    e.to_string(),
                ))))
            }
        }
    }
}

#[async_trait]
impl CopyHandler for PgHandler {
    async fn on_copy_data<C>(&self, _client: &mut C, copy_data: CopyData) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let (parse_res, table_name, started_txn) = {
            let mut ctx_guard = self.copy_context.lock().await;
            let Some(ctx) = ctx_guard.as_mut() else {
                return Ok(());
            };

            let table_name = ctx.table_name.clone();
            let started_txn = ctx.started_txn;

            let parse_res = (|| -> PgWireResult<Vec<Vec<(String, Value)>>> {
                if ctx.reached_end_marker {
                    return Ok(Vec::new());
                }

                let lines = ctx.push_copy_data(copy_data.data.as_ref())?;
                if lines.is_empty() {
                    return Ok(Vec::new());
                }

                let mut rows_to_insert: Vec<Vec<(String, Value)>> = Vec::with_capacity(lines.len());
                for line_bytes in lines {
                    if ctx.reached_end_marker {
                        break;
                    }
                    if line_bytes.as_slice() == b"\\." {
                        ctx.reached_end_marker = true;
                        ctx.line_buffer.clear();
                        break;
                    }

                    let line = String::from_utf8_lossy(&line_bytes);
                    let values: Vec<&str> = line.split('\t').collect();

                    if values.len() != ctx.columns.len() {
                        return Err(copy_row_column_mismatch_error(
                            values.len(),
                            ctx.columns.len(),
                        ));
                    }

                    let mut col_values: Vec<(String, Value)> =
                        Vec::with_capacity(ctx.columns.len());
                    for ((col_name, col_type), val) in ctx
                        .columns
                        .iter()
                        .zip(ctx.column_types.iter())
                        .zip(values.iter())
                    {
                        let value = if *val == "\\N" {
                            Value::Null
                        } else if let Some(dt) = col_type.as_ref() {
                            self.executor.parse_value_for_copy(val, dt).map_err(|e| {
                                PgWireError::UserError(Box::new(ErrorInfo::new(
                                    "ERROR".to_string(),
                                    "22P02".to_string(),
                                    e.to_string(),
                                )))
                            })?
                        } else {
                            Value::Text(val.to_string())
                        };
                        col_values.push((col_name.clone(), value));
                    }
                    rows_to_insert.push(col_values);
                }

                Ok(rows_to_insert)
            })();

            (parse_res, table_name, started_txn)
        };

        let rows_to_insert = match parse_res {
            Ok(rows) => rows,
            Err(e) => {
                let mut ctx_guard = self.copy_context.lock().await;
                *ctx_guard = None;
                drop(ctx_guard);

                let mut session = self.session.lock().await;
                rollback_autocommit_or_mark_failed(&mut session, started_txn).await;
                return Err(e);
            }
        };

        let inserted_count = rows_to_insert.len();
        if inserted_count == 0 {
            return Ok(());
        }

        let insert_res: PgWireResult<()> = async {
            let mut session = self.session.lock().await;

            if session.is_transaction_failed() {
                return Err(in_failed_sql_transaction_pgwire_error());
            }

            let savepoints = session.savepoints();
            crate::txn::with_savepoints(savepoints, async {
                for col_values in rows_to_insert {
                    self.executor
                        .execute_copy_insert(&mut session, &table_name, col_values)
                        .await
                        .map_err(|e| {
                            error!("COPY insert error: {}", e);
                            PgWireError::UserError(Box::new(ErrorInfo::new(
                                "ERROR".to_string(),
                                "XX000".to_string(),
                                e.to_string(),
                            )))
                        })?;
                }

                Ok::<(), PgWireError>(())
            })
            .await?;

            Ok(())
        }
        .await;

        if let Err(e) = insert_res {
            let mut session = self.session.lock().await;
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
            let mut session = self.session.lock().await;

            if session.is_transaction_failed() {
                return Err(in_failed_sql_transaction_pgwire_error());
            }

            if let Some(final_line_bytes) = ctx.drain_final_line() {
                if final_line_bytes.as_slice() == b"\\." {
                    ctx.reached_end_marker = true;
                } else if !ctx.reached_end_marker {
                    let line = String::from_utf8_lossy(&final_line_bytes);
                    let values: Vec<&str> = line.split('\t').collect();

                    if values.len() != ctx.columns.len() {
                        rollback_autocommit_or_mark_failed(&mut session, ctx.started_txn).await;
                        return Err(copy_row_column_mismatch_error(
                            values.len(),
                            ctx.columns.len(),
                        ));
                    }

                    let mut col_values: Vec<(String, Value)> =
                        Vec::with_capacity(ctx.columns.len());
                    for ((col_name, col_type), val) in ctx
                        .columns
                        .iter()
                        .zip(ctx.column_types.iter())
                        .zip(values.iter())
                    {
                        let value = if *val == "\\N" {
                            Value::Null
                        } else if let Some(dt) = col_type.as_ref() {
                            self.executor.parse_value_for_copy(val, dt).map_err(|e| {
                                PgWireError::UserError(Box::new(ErrorInfo::new(
                                    "ERROR".to_string(),
                                    "22P02".to_string(),
                                    e.to_string(),
                                )))
                            })?
                        } else {
                            Value::Text(val.to_string())
                        };
                        col_values.push((col_name.clone(), value));
                    }

                    let savepoints = session.savepoints();
                    let insert_res = crate::txn::with_savepoints(savepoints, async {
                        self.executor
                            .execute_copy_insert(&mut session, &ctx.table_name, col_values)
                            .await
                            .map_err(|e| {
                                error!("COPY insert error: {}", e);
                                PgWireError::UserError(Box::new(ErrorInfo::new(
                                    "ERROR".to_string(),
                                    "XX000".to_string(),
                                    e.to_string(),
                                )))
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
                session.commit().await.map_err(|e| {
                    PgWireError::UserError(Box::new(ErrorInfo::new(
                        "ERROR".to_string(),
                        "XX000".to_string(),
                        e.to_string(),
                    )))
                })?;
            }

            ctx.row_count
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
            let mut session = self.session.lock().await;
            rollback_autocommit_or_mark_failed(&mut session, ctx.started_txn).await;
        }

        warn!("COPY failed: {}", fail.message);

        PgWireError::UserError(Box::new(ErrorInfo::new(
            "ERROR".to_owned(),
            "XX000".to_owned(),
            format!("COPY IN mode terminated: {}", fail.message),
        )))
    }
}

#[async_trait]
impl ExtendedQueryHandler for PgHandler {
    type Statement = String;
    type QueryParser = TipgQueryParser;

    fn query_parser(&self) -> Arc<Self::QueryParser> {
        self.query_parser.clone()
    }

    async fn on_execute<C>(
        &self,
        client: &mut C,
        message: pgwire::messages::extendedquery::Execute,
    ) -> PgWireResult<()>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: PortalStore<Statement = Self::Statement>,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        on_execute_with_tx_status_fix(self, &self.suspended_portals, client, message).await
    }

    async fn on_bind<C>(
        &self,
        client: &mut C,
        message: pgwire::messages::extendedquery::Bind,
    ) -> PgWireResult<()>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: PortalStore<Statement = Self::Statement>,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let portal_name = message
            .portal_name
            .as_deref()
            .unwrap_or(pgwire::api::DEFAULT_NAME);
        {
            let mut guard = self.suspended_portals.lock().await;
            guard.remove(portal_name);
        }

        let statement_name = message
            .statement_name
            .as_deref()
            .unwrap_or(pgwire::api::DEFAULT_NAME);

        if let Some(mut statement) = client.portal_store().get_statement(statement_name) {
            // Some clients (e.g. pgx/GORM) omit parameter type OIDs in Parse and expect the server
            // to infer types. Ensure the portal references a statement with inferred types so that
            // binary parameters are decoded correctly during execution.
            let param_count = message.parameters.len();
            let needs_inference = param_count > 0
                && (statement.parameter_types.len() < param_count
                    || statement
                        .parameter_types
                        .iter()
                        .take(param_count)
                        .any(|t| *t == Type::UNKNOWN));

            if needs_inference {
                if let Ok(describe_response) =
                    self.do_describe_statement(client, statement.as_ref()).await
                {
                    if describe_response.parameters.len() >= param_count
                        && describe_response.parameters != statement.parameter_types
                    {
                        let updated = Arc::new(StoredStatement::new(
                            statement.id.clone(),
                            statement.statement.clone(),
                            describe_response.parameters,
                        ));
                        client.portal_store().put_statement(updated.clone());
                        statement = updated;
                    }
                }
            }

            let portal = Portal::try_new(&message, statement)?;
            client.portal_store().put_portal(Arc::new(portal));
            client
                .send(PgWireBackendMessage::BindComplete(
                    pgwire::messages::extendedquery::BindComplete::new(),
                ))
                .await?;
            Ok(())
        } else {
            Err(PgWireError::StatementNotFound(statement_name.to_owned()))
        }
    }

    async fn on_close<C>(
        &self,
        client: &mut C,
        message: pgwire::messages::extendedquery::Close,
    ) -> PgWireResult<()>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: PortalStore<Statement = Self::Statement>,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let name = message.name.as_deref().unwrap_or(pgwire::api::DEFAULT_NAME);
        match message.target_type {
            pgwire::messages::extendedquery::TARGET_TYPE_BYTE_STATEMENT => {
                client.portal_store().rm_statement(name);
            }
            pgwire::messages::extendedquery::TARGET_TYPE_BYTE_PORTAL => {
                client.portal_store().rm_portal(name);
                let mut guard = self.suspended_portals.lock().await;
                guard.remove(name);
            }
            _ => {}
        }

        client
            .send(PgWireBackendMessage::CloseComplete(
                pgwire::messages::extendedquery::CloseComplete::new(),
            ))
            .await?;
        Ok(())
    }

    async fn do_query<'a, 'b: 'a, C>(
        &'b self,
        client: &mut C,
        portal: &'a Portal<Self::Statement>,
        _max_rows: usize,
    ) -> PgWireResult<Response<'a>>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let query = &portal.statement.statement;
        debug!("Extended query: {}", query);

        let final_query = substitute_parameters(query, portal)?;
        debug!("Final query after substitution: {}", final_query);

        let mut session = self.session.lock().await;

        set_connection_id(session.connection_id());

        match self.executor.execute(&mut session, &final_query).await {
            Ok(results) => Ok(send_notices_and_get_last_response(
                client,
                session.show_setting_value("client_min_messages"),
                results,
            )
            .await?),
            Err(e) => {
                error!("Extended query execution error: {}", e);
                Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                    "ERROR".to_string(),
                    sqlstate_for_executor_error(&e).to_string(),
                    e.to_string(),
                ))))
            }
        }
    }

    async fn do_describe_statement<C>(
        &self,
        _client: &mut C,
        stmt: &StoredStatement<Self::Statement>,
    ) -> PgWireResult<DescribeStatementResponse>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        let param_count = count_sql_parameters(&stmt.statement);
        let mut param_types: Vec<Type> = stmt.parameter_types.clone();

        if param_types.len() < param_count || param_types.iter().any(|t| *t == Type::UNKNOWN) {
            let inferred = infer_parameter_types(&stmt.statement, param_count);
            for i in param_types.len()..param_count {
                param_types.push(inferred[i].clone());
            }
            for i in 0..param_types.len().min(inferred.len()) {
                if param_types[i] == Type::UNKNOWN && inferred[i] != Type::UNKNOWN {
                    param_types[i] = inferred[i].clone();
                }
            }
        }

        let query_for_inference = if param_count == 0 {
            stmt.statement.clone()
        } else {
            let mut values: Vec<String> = Vec::with_capacity(param_count);
            for i in 0..param_count {
                let t = param_types.get(i).cloned().unwrap_or(Type::UNKNOWN);
                values.push(dummy_sql_expr_for_param_type(&t));
            }
            substitute_placeholders_outside_strings_and_dollar(&stmt.statement, &values)
        };
        let fields = self
            .infer_result_fields_from_query(&query_for_inference)
            .await;
        Ok(DescribeStatementResponse::new(param_types, fields))
    }

    async fn do_describe_portal<C>(
        &self,
        _client: &mut C,
        portal: &Portal<Self::Statement>,
    ) -> PgWireResult<DescribePortalResponse>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        let final_query = substitute_parameters(&portal.statement.statement, portal)?;
        let fields = self.infer_result_fields_from_query(&final_query).await;
        Ok(DescribePortalResponse::new(fields))
    }
}

impl PgWireServerHandlers for HandlerFactory {
    type StartupHandler = PgHandler;
    type SimpleQueryHandler = PgHandler;
    type ExtendedQueryHandler = PgHandler;
    type CopyHandler = PgHandler;
    type ErrorHandler = NoopErrorHandler;

    fn simple_query_handler(&self) -> Arc<Self::SimpleQueryHandler> {
        self.handler.clone()
    }

    fn extended_query_handler(&self) -> Arc<Self::ExtendedQueryHandler> {
        self.handler.clone()
    }

    fn startup_handler(&self) -> Arc<Self::StartupHandler> {
        self.handler.clone()
    }

    fn copy_handler(&self) -> Arc<Self::CopyHandler> {
        self.handler.clone()
    }

    fn error_handler(&self) -> Arc<Self::ErrorHandler> {
        Arc::new(NoopErrorHandler)
    }
}
