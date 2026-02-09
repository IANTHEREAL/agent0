use super::copy::copy_row_column_mismatch_error;
use super::encode::{datatype_to_pgtype, result_to_response};
use super::errors::{
    ambiguous_column_error_with_position, in_failed_sql_transaction_pgwire_error,
    sqlstate_for_executor_error,
};
use super::params::{
    count_sql_parameters, dummy_sql_expr_for_param_type, infer_parameter_types,
    substitute_parameters, substitute_placeholders_outside_strings_and_dollar,
};
use super::portal::{
    on_execute_with_tx_status_fix, on_query_with_tx_status_fix, SuspendedPortalState,
};
use super::query_parser::strip_leading_whitespace_and_comments;
use super::tenant::parse_tenant_username;
use super::{
    client_allows_notice, count_placeholders_in_expr, extract_placeholder_index_from_expr,
    infer_result_fields_from_query_ast, infer_types_from_expr, parse_startup_options,
    resolve_copy_columns, resolve_table_for_insert, rollback_autocommit_or_mark_failed,
    send_notices_and_get_last_response, stub_describe_field, CopyContext,
    PgServerParameterProvider, TipgQueryParser, CONNECTION_ID_COUNTER, METADATA_ACTUAL_USER,
    METADATA_AUTH_IS_SUPERUSER, METADATA_KEYSPACE,
};
use crate::auth::AuthManager;
use crate::observability;
use crate::pool::{TenantHandle, TikvClientPool};
use crate::sql::expr::set_connection_id;
use crate::sql::{ExecuteResult, Executor, Session};
use crate::storage::TikvStore;
use crate::types::{DataType, TableSchema, Value};
use async_trait::async_trait;
use futures::{Sink, SinkExt};
use pgwire::api::auth::StartupHandler;
use pgwire::api::copy::CopyHandler;
use pgwire::api::portal::Portal;
use pgwire::api::query::{ExtendedQueryHandler, SimpleQueryHandler};
use pgwire::api::results::{
    CopyResponse, DescribePortalResponse, DescribeStatementResponse, FieldFormat, FieldInfo,
    Response,
};
use pgwire::api::stmt::StoredStatement;
use pgwire::api::store::PortalStore;
use pgwire::api::{
    ClientInfo, ClientPortalStore, NoopErrorHandler, PgWireConnectionState, PgWireServerHandlers,
    Type, METADATA_DATABASE, METADATA_USER,
};
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::copy::{CopyData, CopyDone, CopyFail};
use pgwire::messages::response::{CommandComplete, ErrorResponse, NoticeResponse};
use pgwire::messages::startup::Authentication;
use pgwire::messages::{PgWireBackendMessage, PgWireFrontendMessage};
use sqlparser::ast::{CopySource, CopyTarget, Expr, Ident, Statement};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::sync::{Mutex, OnceCell};
use tracing::{debug, error, info, warn};

pub struct DynamicPgHandler {
    client_pool: Option<Arc<TikvClientPool>>,
    pd_endpoints: Vec<String>,
    default_keyspace: Option<String>,
    executor: OnceCell<Arc<Executor>>,
    session: Mutex<Option<Session>>,
    connection_guard: OnceCell<observability::ConnectionGuard>,
    tenant_handle: OnceCell<TenantHandle>,
    copy_context: Mutex<Option<CopyContext>>,
    suspended_portals: Mutex<HashMap<String, SuspendedPortalState>>,
    query_parser: Arc<TipgQueryParser>,
    connection_id: i32,
}

impl DynamicPgHandler {
    #[allow(dead_code)]
    pub fn new(pd_endpoints: Vec<String>, default_keyspace: Option<String>) -> Self {
        Self {
            client_pool: None,
            pd_endpoints,
            default_keyspace,
            executor: OnceCell::new(),
            session: Mutex::new(None),
            connection_guard: OnceCell::new(),
            tenant_handle: OnceCell::new(),
            copy_context: Mutex::new(None),
            suspended_portals: Mutex::new(HashMap::new()),
            query_parser: Arc::new(TipgQueryParser::new()),
            connection_id: CONNECTION_ID_COUNTER.fetch_add(1, Ordering::Relaxed),
        }
    }

    pub fn new_with_pool(
        client_pool: Arc<TikvClientPool>,
        default_keyspace: Option<String>,
    ) -> Self {
        Self {
            client_pool: Some(client_pool),
            pd_endpoints: Vec::new(),
            default_keyspace,
            executor: OnceCell::new(),
            session: Mutex::new(None),
            connection_guard: OnceCell::new(),
            tenant_handle: OnceCell::new(),
            copy_context: Mutex::new(None),
            suspended_portals: Mutex::new(HashMap::new()),
            query_parser: Arc::new(TipgQueryParser::new()),
            connection_id: CONNECTION_ID_COUNTER.fetch_add(1, Ordering::Relaxed),
        }
    }

    #[allow(dead_code)]
    pub fn connection_id(&self) -> i32 {
        self.connection_id
    }

    async fn infer_insert_parameter_types(
        &self,
        sql: &str,
        param_count: usize,
    ) -> Option<Vec<Type>> {
        if param_count == 0 {
            return Some(vec![]);
        }

        let parsed = crate::sql::parse_sql(sql).ok()?;
        let stmt = parsed.into_iter().next()?;

        let (table_name, columns, values_list): (String, Vec<String>, Vec<Vec<Expr>>) = match stmt {
            Statement::Insert {
                table_name,
                columns,
                source,
                ..
            } => {
                let table_name_str = table_name.to_string();
                let columns: Vec<String> = columns.iter().map(|c| c.value.clone()).collect();
                let values = match source.as_ref() {
                    Some(src) => src,
                    None => return None,
                };
                if let sqlparser::ast::SetExpr::Values(v) = values.body.as_ref() {
                    (table_name_str, columns, v.rows.clone())
                } else {
                    return None;
                }
            }
            _ => return None,
        };

        info!(
            "infer_insert_parameter_types: table={}, columns={:?}, param_count={}",
            table_name, columns, param_count
        );

        let executor = self.executor.get()?;
        let store = executor.store();

        let mut session_guard = self.session.lock().await;
        let session = session_guard.as_mut()?;
        let mut txn = store.begin().await.ok()?;

        let search_path = session.search_path();
        let resolved_table = resolve_table_for_insert(&table_name, search_path);

        info!(
            "infer_insert_parameter_types: resolved_table={}",
            resolved_table
        );

        let schema = store
            .get_schema(&mut txn, session.current_database_id(), &resolved_table)
            .await
            .ok()??;
        let _ = txn.rollback().await;

        info!(
            "infer_insert_parameter_types: got schema with {} columns",
            schema.columns.len()
        );

        let col_types: std::collections::HashMap<String, DataType> = schema
            .columns
            .iter()
            .map(|c| (c.name.to_lowercase(), c.data_type.clone()))
            .collect();

        let column_order: Vec<String> = if !columns.is_empty() {
            columns.iter().map(|c: &String| c.to_lowercase()).collect()
        } else {
            schema
                .columns
                .iter()
                .map(|c| c.name.to_lowercase())
                .collect()
        };

        let mut types = vec![Type::TEXT; param_count];

        if let Some(first_row) = values_list.first() {
            let mut param_idx = 0usize;
            for (col_idx, expr) in first_row.iter().enumerate() {
                let col_name = column_order.get(col_idx)?;
                let col_type = col_types.get(col_name);

                let placeholders = count_placeholders_in_expr(expr);
                for _ in 0..placeholders {
                    if param_idx < param_count {
                        types[param_idx] = datatype_to_pgtype(col_type);
                        param_idx += 1;
                    }
                }
            }
        }

        Some(types)
    }

    async fn infer_update_parameter_types(
        &self,
        sql: &str,
        param_count: usize,
    ) -> Option<Vec<Type>> {
        if param_count == 0 {
            return Some(vec![]);
        }

        let parsed = crate::sql::parse_sql(sql).ok()?;
        let stmt = parsed.into_iter().next()?;

        let (table_name, assignments) = match stmt {
            Statement::Update {
                table, assignments, ..
            } => {
                let table_name = match &table.relation {
                    sqlparser::ast::TableFactor::Table { name, .. } => name.to_string(),
                    _ => return None,
                };
                (table_name, assignments)
            }
            _ => return None,
        };

        info!(
            "infer_update_parameter_types: table={}, assignments={}, param_count={}",
            table_name,
            assignments.len(),
            param_count
        );

        let executor = self.executor.get()?;
        let store = executor.store();

        let mut session_guard = self.session.lock().await;
        let session = session_guard.as_mut()?;
        let mut txn = store.begin().await.ok()?;

        let search_path = session.search_path();
        let resolved_table = resolve_table_for_insert(&table_name, search_path);

        info!(
            "infer_update_parameter_types: resolved_table={}",
            resolved_table
        );

        let schema = store
            .get_schema(&mut txn, session.current_database_id(), &resolved_table)
            .await
            .ok()??;
        let _ = txn.rollback().await;

        info!(
            "infer_update_parameter_types: got schema with {} columns",
            schema.columns.len()
        );

        let col_types: std::collections::HashMap<String, DataType> = schema
            .columns
            .iter()
            .map(|c| (c.name.to_lowercase(), c.data_type.clone()))
            .collect();

        let mut types = vec![Type::TEXT; param_count];
        let mut param_idx = 0usize;

        for assignment in &assignments {
            let col_names: Vec<String> = assignment
                .id
                .iter()
                .map(|ident| ident.value.to_lowercase())
                .collect();

            if let Some(col_name) = col_names.last() {
                let col_type = col_types.get(col_name);
                let placeholders = count_placeholders_in_expr(&assignment.value);

                for _ in 0..placeholders {
                    if param_idx < param_count {
                        types[param_idx] = datatype_to_pgtype(col_type);
                        param_idx += 1;
                    }
                }
            }
        }

        info!(
            "infer_update_parameter_types: inferred {} types, remaining {} as TEXT",
            param_idx,
            param_count - param_idx
        );

        Some(types)
    }

    async fn infer_select_parameter_types(
        &self,
        sql: &str,
        param_count: usize,
    ) -> Option<Vec<Type>> {
        if param_count == 0 {
            return Some(vec![]);
        }

        let parsed = crate::sql::parse_sql(sql).ok()?;
        let stmt = parsed.into_iter().next()?;

        let (table_name, selection, limit_expr, offset_expr) = match stmt {
            Statement::Query(query) => {
                if let sqlparser::ast::SetExpr::Select(select) = query.body.as_ref() {
                    let table_name = select.from.first().and_then(|f| match &f.relation {
                        sqlparser::ast::TableFactor::Table { name, .. } => Some(name.to_string()),
                        _ => None,
                    })?;
                    (
                        table_name,
                        select.selection.clone(),
                        query.limit.clone(),
                        query.offset.clone(),
                    )
                } else {
                    return None;
                }
            }
            _ => return None,
        };

        info!(
            "infer_select_parameter_types: table={}, param_count={}",
            table_name, param_count
        );

        let executor = self.executor.get()?;
        let store = executor.store();

        let mut session_guard = self.session.lock().await;
        let session = session_guard.as_mut()?;
        let mut txn = store.begin().await.ok()?;

        let search_path = session.search_path();
        let resolved_table = resolve_table_for_insert(&table_name, search_path);

        let schema = store
            .get_schema(&mut txn, session.current_database_id(), &resolved_table)
            .await
            .ok()??;
        let _ = txn.rollback().await;

        let col_types: std::collections::HashMap<String, DataType> = schema
            .columns
            .iter()
            .map(|c| (c.name.to_lowercase(), c.data_type.clone()))
            .collect();

        let mut types = vec![Type::TEXT; param_count];

        // Infer types from WHERE clause
        if let Some(ref sel) = selection {
            infer_types_from_expr(sel, &col_types, &mut types);
        }

        // LIMIT placeholder should be INT8
        if let Some(ref limit) = limit_expr {
            if let Some(idx) = extract_placeholder_index_from_expr(limit) {
                if idx < types.len() {
                    types[idx] = Type::INT8;
                }
            }
        }

        // OFFSET placeholder should be INT8
        if let Some(ref offset) = offset_expr {
            if let Some(idx) = extract_placeholder_index_from_expr(&offset.value) {
                if idx < types.len() {
                    types[idx] = Type::INT8;
                }
            }
        }

        info!("infer_select_parameter_types: inferred types={:?}", types);

        Some(types)
    }

    async fn infer_result_fields_from_query(&self, query: &str) -> Vec<FieldInfo> {
        // Get the executor if initialized
        let executor = match self.executor.get() {
            Some(exec) => exec,
            None => {
                // Executor not initialized yet, return stub
                return stub_describe_field();
            }
        };

        // Get session lock
        let mut session_guard = self.session.lock().await;

        // Check if session exists
        if session_guard.is_none() {
            // No session yet, return stub
            return stub_describe_field();
        }

        let store = executor.store();
        let session = session_guard.as_mut().unwrap();
        infer_result_fields_from_query_ast(&store, session, query).await
    }

    async fn init_executor(
        &self,
        keyspace: Option<String>,
        username: Option<String>,
        is_superuser: bool,
        database: String,
    ) -> Result<(), String> {
        let effective_keyspace = keyspace
            .or_else(|| self.default_keyspace.clone())
            .unwrap_or_else(|| "default".to_string());

        let tenant_obs = observability::registry().tenant(&effective_keyspace);
        if self.connection_guard.get().is_none() {
            let _ = self.connection_guard.set(tenant_obs.connection_open());
        }

        let store = if let Some(pool) = &self.client_pool {
            let handle = pool
                .acquire(Some(effective_keyspace.clone()))
                .await
                .map_err(|e| format!("Failed to get client from pool: {}", e))?;
            let s = handle.store().clone();
            let _ = self.tenant_handle.set(handle);
            s
        } else {
            let s = TikvStore::new_with_keyspace(
                self.pd_endpoints.clone(),
                Some(effective_keyspace.clone()),
            )
            .await
            .map_err(|e| format!("Failed to connect to TiKV: {}", e))?;
            Arc::new(s)
        };

        let executor = Arc::new(Executor::new(
            store.clone(),
            effective_keyspace.clone(),
            tenant_obs.clone(),
        ));

        let database_name = database.trim();
        let database_name = if database_name.is_empty() {
            "postgres"
        } else {
            database_name
        };
        let database_name = database_name.to_ascii_lowercase();

        let mut db_txn = store.begin().await.map_err(|e| e.to_string())?;
        let database_id = match store
            .get_database_id(&mut db_txn, &database_name)
            .await
            .map_err(|e| e.to_string())?
        {
            Some(id) => id,
            None => {
                db_txn.rollback().await.ok();
                return Err(format!("database \"{}\" does not exist", database_name));
            }
        };
        db_txn.rollback().await.ok();

        let session = match username {
            Some(user) => Session::new_with_user_and_database(
                store,
                tenant_obs,
                user,
                is_superuser,
                self.connection_id,
                database_id,
                database_name,
            ),
            None => Session::new_with_database(
                store,
                tenant_obs,
                self.connection_id,
                database_id,
                database_name,
            ),
        };

        let _ = self.executor.set(executor);

        let mut session_guard = self.session.lock().await;
        *session_guard = Some(session);

        debug!(
            "Initialized executor with keyspace: {:?}",
            effective_keyspace
        );
        Ok(())
    }

    fn get_executor(&self) -> Result<&Arc<Executor>, PgWireError> {
        self.executor.get().ok_or_else(|| {
            PgWireError::UserError(Box::new(ErrorInfo::new(
                "FATAL".to_string(),
                "XX000".to_string(),
                "Executor not initialized - authentication required".to_string(),
            )))
        })
    }

    pub(super) fn parse_copy_command(query: &str) -> Option<(String, Vec<String>)> {
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

    pub(super) fn parse_copy_to_command(
        query: &str,
    ) -> Result<Option<(String, Vec<String>)>, ErrorInfo> {
        fn unsupported_copy_to_stdout_syntax() -> ErrorInfo {
            ErrorInfo::new(
                "ERROR".to_string(),
                "0A000".to_string(),
                "Unsupported COPY TO STDOUT syntax. Supported: COPY [schema.]table [(col1, col2, ...)] TO STDOUT".to_string(),
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

        if stmts.len() != 1
            || !options.is_empty()
            || !legacy_options.is_empty()
            || !values.is_empty()
        {
            return Err(unsupported_copy_to_stdout_syntax());
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

        Ok(Some((table_name, columns)))
    }

    fn copy_out_response_from_select_result(
        result: ExecuteResult,
    ) -> Result<(CopyResponse, Vec<crate::types::Row>), ErrorInfo> {
        match result {
            ExecuteResult::Select { columns, rows, .. } => {
                let col_count = columns.len();
                let column_formats: Vec<i16> = vec![0; col_count];
                Ok((CopyResponse::new(0, col_count, column_formats), rows))
            }
            _ => Err(ErrorInfo::new(
                "ERROR".to_string(),
                "0A000".to_string(),
                "COPY TO STDOUT is only supported for tables".to_string(),
            )),
        }
    }

    async fn handle_copy_to_stdout<'a, C>(
        &self,
        client: &mut C,
        table_name: &str,
        columns: &[String],
    ) -> PgWireResult<Vec<Response<'a>>>
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

        let (copy_resp, rows) = Self::copy_out_response_from_select_result(result)
            .map_err(|e| PgWireError::UserError(Box::new(e)))?;

        drop(session_guard);

        pgwire::api::copy::send_copy_out_response(client, copy_resp).await?;

        let mut buf = Vec::with_capacity(4096);
        for row in &rows {
            buf.clear();
            crate::protocol::copy_format::encode_row(&row.values, &mut buf);
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

    pub(super) fn ensure_auth_bootstrapped(
        bootstrap_result: Result<(), anyhow::Error>,
    ) -> Result<(), String> {
        bootstrap_result.map_err(|e| format!("Failed to bootstrap auth: {}", e))
    }

    async fn authenticate_user(
        &self,
        keyspace: &Option<String>,
        username: &str,
        password: &str,
    ) -> Result<(bool, bool), String> {
        let effective_keyspace = keyspace
            .clone()
            .or_else(|| self.default_keyspace.clone())
            .or_else(|| Some("default".to_string()));

        let ks_name = effective_keyspace
            .clone()
            .unwrap_or_else(|| "default".to_string());

        let store = if let Some(pool) = &self.client_pool {
            match pool.get_client(effective_keyspace).await {
                Ok(s) => s,
                Err(e) => {
                    let err_str = e.to_string();
                    if err_str.contains("does not exist") {
                        error!("Tenant '{}' does not exist (user: {})", ks_name, username);
                    } else {
                        error!("Failed to connect to TiKV for tenant '{}': {}", ks_name, e);
                    }
                    return Ok((false, false));
                }
            }
        } else {
            match TikvStore::new_with_keyspace(self.pd_endpoints.clone(), effective_keyspace).await
            {
                Ok(s) => Arc::new(s),
                Err(e) => {
                    error!("Failed to connect to TiKV: {}", e);
                    return Ok((false, false));
                }
            }
        };

        let auth_manager = AuthManager::new();

        // Try bootstrap. Any failure must deny authentication.
        let bootstrap_result = async {
            let mut txn = store.begin().await?;
            auth_manager.bootstrap(&mut txn).await?;
            txn.commit().await?;
            Ok::<(), anyhow::Error>(())
        }
        .await;

        Self::ensure_auth_bootstrapped(bootstrap_result)?;

        let mut txn = store
            .begin()
            .await
            .map_err(|e| format!("Failed to begin transaction: {}", e))?;

        match auth_manager
            .authenticate(&mut txn, username, password)
            .await
        {
            Ok(Some(user)) => {
                txn.commit()
                    .await
                    .map_err(|e| format!("Failed to commit: {}", e))?;
                Ok((true, user.is_superuser))
            }
            Ok(None) => {
                txn.rollback().await.ok();
                Ok((false, false))
            }
            Err(e) => {
                txn.rollback().await.ok();
                Err(format!("Authentication error: {}", e))
            }
        }
    }
}

#[async_trait]
impl StartupHandler for DynamicPgHandler {
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
        match message {
            PgWireFrontendMessage::Startup(ref startup) => {
                pgwire::api::auth::save_startup_parameters_to_metadata(client, startup);

                if let Some(raw_user) = client.metadata().get(METADATA_USER).cloned() {
                    let (keyspace, actual_user) = parse_tenant_username(&raw_user);

                    if let Some(ks) = &keyspace {
                        client
                            .metadata_mut()
                            .insert(METADATA_KEYSPACE.to_string(), ks.clone());
                        debug!("Extracted keyspace '{}' from username '{}'", ks, raw_user);
                    }
                    client
                        .metadata_mut()
                        .insert(METADATA_ACTUAL_USER.to_string(), actual_user.clone());
                    debug!("Actual user: {}", actual_user);
                }

                client.set_state(PgWireConnectionState::AuthenticationInProgress);
                client
                    .send(PgWireBackendMessage::Authentication(
                        Authentication::CleartextPassword,
                    ))
                    .await?;
            }
            PgWireFrontendMessage::PasswordMessageFamily(pwd) => {
                let pwd = pwd.into_password()?;
                let provided_password = pwd.password.clone();
                let keyspace = client.metadata().get(METADATA_KEYSPACE).cloned();
                let actual_user = client
                    .metadata()
                    .get(METADATA_ACTUAL_USER)
                    .cloned()
                    .unwrap_or_else(|| "admin".to_string());
                let database = client
                    .metadata()
                    .get(METADATA_DATABASE)
                    .cloned()
                    .unwrap_or_else(|| "postgres".to_string());

                let auth_result = self
                    .authenticate_user(&keyspace, &actual_user, &provided_password)
                    .await;

                match auth_result {
                    Ok((is_authenticated, is_superuser)) => {
                        if is_authenticated {
                            if let Err(e) = self
                                .init_executor(
                                    keyspace.clone(),
                                    Some(actual_user.clone()),
                                    is_superuser,
                                    database,
                                )
                                .await
                            {
                                let error_info =
                                    ErrorInfo::new("FATAL".to_owned(), "XX000".to_owned(), e);
                                client
                                    .feed(PgWireBackendMessage::ErrorResponse(ErrorResponse::from(
                                        error_info,
                                    )))
                                    .await?;
                                client.close().await?;
                                return Ok(());
                            }

                            let mut session_guard = self.session.lock().await;
                            if let Some(session) = session_guard.as_mut() {
                                if let Some(options) = client.metadata().get("options") {
                                    for (key, value) in parse_startup_options(options) {
                                        if let Err(e) = session
                                            .set_known_setting(&key.to_ascii_lowercase(), value)
                                        {
                                            warn!("Failed to apply startup option {}: {}", key, e);
                                        }
                                    }
                                }

                                if let Some(app_name) = client.metadata().get("application_name") {
                                    if let Err(e) = session
                                        .set_known_setting("application_name", app_name.clone())
                                    {
                                        warn!(
                                            "Failed to apply application_name from startup: {}",
                                            e
                                        );
                                    }
                                }
                            }

                            client.metadata_mut().insert(
                                METADATA_AUTH_IS_SUPERUSER.to_string(),
                                if is_superuser { "on" } else { "off" }.to_string(),
                            );

                            pgwire::api::auth::finish_authentication(
                                client,
                                &PgServerParameterProvider,
                            )
                            .await?;
                            let peer = client.socket_addr();
                            info!(
                                "New connection from {}:{} user='{}' keyspace='{}'",
                                peer.ip(),
                                peer.port(),
                                actual_user,
                                keyspace.as_deref().unwrap_or("default"),
                            );
                        } else {
                            let error_info = ErrorInfo::new(
                                "FATAL".to_owned(),
                                "28P01".to_owned(),
                                format!(
                                    "Password authentication failed for user \"{}\"",
                                    actual_user
                                ),
                            );
                            client
                                .feed(PgWireBackendMessage::ErrorResponse(ErrorResponse::from(
                                    error_info,
                                )))
                                .await?;
                            client.close().await?;
                        }
                    }
                    Err(e) => {
                        let error_info = ErrorInfo::new("FATAL".to_owned(), "XX000".to_owned(), e);
                        client
                            .feed(PgWireBackendMessage::ErrorResponse(ErrorResponse::from(
                                error_info,
                            )))
                            .await?;
                        client.close().await?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[async_trait]
impl SimpleQueryHandler for DynamicPgHandler {
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

        let executor = self.get_executor()?;

        match Self::parse_copy_to_command(query) {
            Ok(Some((table_name, columns))) => {
                debug!(
                    "COPY TO STDOUT: table={}, columns={:?}",
                    table_name, columns
                );
                return self
                    .handle_copy_to_stdout(client, &table_name, &columns)
                    .await;
            }
            Ok(None) => {}
            Err(e) => return Err(PgWireError::UserError(Box::new(e))),
        }

        if let Some((table_name, columns)) = Self::parse_copy_command(query) {
            debug!(
                "COPY FROM STDIN: table={}, columns={:?}",
                table_name, columns
            );

            let (resolved_table, resolved_columns, column_types, col_count, started_txn) = {
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
                        match executor
                            .store()
                            .get_schema(txn, db_id, &resolved_table)
                            .await
                        {
                            Ok(Some(schema)) => (resolved_table, schema),
                            Ok(None) => {
                                rollback_autocommit_or_mark_failed(session, started_txn).await;
                                return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                                    "ERROR".to_string(),
                                    "42P01".to_string(),
                                    format!("relation \"{}\" does not exist", table_name),
                                ))));
                            }
                            Err(e) => {
                                rollback_autocommit_or_mark_failed(session, started_txn).await;
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
                            match executor
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
                                    rollback_autocommit_or_mark_failed(session, started_txn).await;
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
                                rollback_autocommit_or_mark_failed(session, started_txn).await;
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
                            rollback_autocommit_or_mark_failed(session, started_txn).await;
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

        let mut session_guard = self.session.lock().await;
        let session = session_guard.as_mut().ok_or_else(|| {
            PgWireError::UserError(Box::new(ErrorInfo::new(
                "ERROR".to_string(),
                "XX000".to_string(),
                "Session not initialized".to_string(),
            )))
        })?;

        set_connection_id(session.connection_id());

        match executor.execute(session, query).await {
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
                let mut error_info = ErrorInfo::new(
                    "ERROR".to_string(),
                    sqlstate_for_executor_error(&e).to_string(),
                    e.to_string(),
                );
                if let Some((_col, pos)) =
                    ambiguous_column_error_with_position(query, &error_info.message)
                {
                    error_info.code = "42702".to_string();
                    error_info.position = Some(pos.to_string());
                }
                Err(PgWireError::UserError(Box::new(error_info)))
            }
        }
    }
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
                            executor.parse_value_for_copy(val, dt).map_err(|e| {
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

        let insert_res: PgWireResult<()> = async {
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
                for col_values in rows_to_insert {
                    executor
                        .execute_copy_insert(session, &table_name, col_values)
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
                    let line = String::from_utf8_lossy(&final_line_bytes);
                    let values: Vec<&str> = line.split('\t').collect();

                    if values.len() != ctx.columns.len() {
                        rollback_autocommit_or_mark_failed(session, ctx.started_txn).await;
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
                            executor.parse_value_for_copy(val, dt).map_err(|e| {
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
                        executor
                            .execute_copy_insert(session, &ctx.table_name, col_values)
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

#[async_trait]
impl ExtendedQueryHandler for DynamicPgHandler {
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
        let executor = self.get_executor()?;
        let query = &portal.statement.statement;
        debug!("Extended query: {}", query);

        let final_query = substitute_parameters(query, portal)?;
        debug!("Final query after substitution: {}", final_query);

        let mut session_guard = self.session.lock().await;
        let session = session_guard.as_mut().ok_or_else(|| {
            PgWireError::UserError(Box::new(ErrorInfo::new(
                "ERROR".to_string(),
                "XX000".to_string(),
                "Session not initialized".to_string(),
            )))
        })?;

        set_connection_id(session.connection_id());

        match executor.execute(session, &final_query).await {
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

        info!(
            "do_describe_statement: sql={}, param_count={}, initial_types={:?}",
            stmt.statement.chars().take(100).collect::<String>(),
            param_count,
            param_types
        );

        if param_types.len() < param_count || param_types.iter().any(|t| *t == Type::UNKNOWN) {
            let inferred = if let Some(insert_types) = self
                .infer_insert_parameter_types(&stmt.statement, param_count)
                .await
            {
                info!(
                    "do_describe_statement: inferred INSERT types={:?}",
                    insert_types
                );
                insert_types
            } else if let Some(update_types) = self
                .infer_update_parameter_types(&stmt.statement, param_count)
                .await
            {
                info!(
                    "do_describe_statement: inferred UPDATE types={:?}",
                    update_types
                );
                update_types
            } else if let Some(select_types) = self
                .infer_select_parameter_types(&stmt.statement, param_count)
                .await
            {
                info!(
                    "do_describe_statement: inferred SELECT types={:?}",
                    select_types
                );
                select_types
            } else {
                let fallback = infer_parameter_types(&stmt.statement, param_count);
                info!("do_describe_statement: fallback types={:?}", fallback);
                fallback
            };
            for i in param_types.len()..param_count {
                param_types.push(inferred[i].clone());
            }
            for i in 0..param_types.len().min(inferred.len()) {
                if param_types[i] == Type::UNKNOWN && inferred[i] != Type::UNKNOWN {
                    param_types[i] = inferred[i].clone();
                }
            }
        }

        info!("do_describe_statement: final_types={:?}", param_types);

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

#[allow(dead_code)]
fn infer_result_fields(query: &str) -> Vec<FieldInfo> {
    let query_upper = query.to_uppercase();
    if query_upper.starts_with("SELECT") {
        vec![FieldInfo::new(
            "column".to_string(),
            None,
            None,
            Type::TEXT,
            FieldFormat::Text,
        )]
    } else {
        vec![]
    }
}

pub struct DynamicHandlerFactory {
    handler: Arc<DynamicPgHandler>,
}

impl DynamicHandlerFactory {
    #[allow(dead_code)]
    pub fn new(pd_endpoints: Vec<String>, default_keyspace: Option<String>) -> Self {
        Self {
            handler: Arc::new(DynamicPgHandler::new(pd_endpoints, default_keyspace)),
        }
    }

    pub fn new_with_pool(
        client_pool: Arc<TikvClientPool>,
        default_keyspace: Option<String>,
    ) -> Self {
        Self {
            handler: Arc::new(DynamicPgHandler::new_with_pool(
                client_pool,
                default_keyspace,
            )),
        }
    }
}

impl PgWireServerHandlers for DynamicHandlerFactory {
    type StartupHandler = DynamicPgHandler;
    type SimpleQueryHandler = DynamicPgHandler;
    type ExtendedQueryHandler = DynamicPgHandler;
    type CopyHandler = DynamicPgHandler;
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
