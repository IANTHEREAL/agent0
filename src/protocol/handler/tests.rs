use super::*;
use crate::sql::InFailedSqlTransaction;
use crate::types::Value;
use async_trait::async_trait;
use bytes::Buf;
use bytes::Bytes;
use pgwire::api::portal::Format;
use pgwire::api::portal::Portal;
use pgwire::api::query::ExtendedQueryHandler;
use pgwire::api::results::DataRowEncoder;
use pgwire::api::stmt::NoopQueryParser;
use pgwire::api::stmt::QueryParser;
use pgwire::api::stmt::StoredStatement;
use pgwire::api::store::PortalStore;
use pgwire::api::DefaultClient;
use pgwire::api::{ClientInfo, ClientPortalStore, PgWireConnectionState, Type};
use pgwire::messages::response::CommandComplete;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll};

use super::encode::encode_value;
use super::errors::sqlstate_for_executor_error;
use super::params::{
    count_sql_parameters, find_keyword_outside_strings, infer_parameter_types,
    replace_placeholders_for_inference, substitute_parameters,
    substitute_placeholders_outside_strings_and_dollar,
};
use super::portal::{
    max_suspended_portal_buffer_rows, max_suspended_portals, on_execute_with_tx_status_fix,
    update_tx_status_after_execution, SuspendedPortalState,
};
use super::tenant::parse_tenant_username;
use futures::stream;
use futures::StreamExt;
use pgwire::api::results::{DescribePortalResponse, DescribeStatementResponse, QueryResponse, Tag};
use pgwire::messages::data::DataRow;
use pgwire::messages::response::TransactionStatus;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

#[derive(Default)]
struct RecordingSink {
    messages: Vec<PgWireBackendMessage>,
}

impl Sink<PgWireBackendMessage> for RecordingSink {
    type Error = PgWireError;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: PgWireBackendMessage) -> Result<(), Self::Error> {
        self.get_mut().messages.push(item);
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

fn test_column(name: &str, data_type: DataType) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        data_type,
        nullable: false,
        primary_key: false,
        unique: false,
        is_serial: false,
        default_expr: None,
    }
}

fn test_schema(name: &str, columns: Vec<ColumnDef>) -> TableSchema {
    TableSchema {
        name: name.to_string(),
        table_id: 0,
        columns,
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        from_alias: None,
    }
}

static FS9_INFER_NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn unique_fs9_infer_base(name: &str) -> PathBuf {
    let id = FS9_INFER_NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = PathBuf::from(format!(
        "/tmp/pgtikv-fs9-infer-test-{name}-{}-{id}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).expect("create test dir");
    dir
}

fn cleanup_dir(path: &PathBuf) {
    let _ = fs::remove_dir_all(path);
}

fn extract_fs9_args(sql: &str) -> Vec<FunctionArg> {
    let mut stmts = crate::sql::parse_sql(sql).expect("parse sql");
    let stmt = stmts.pop().expect("expected statement");
    let Statement::Query(q) = stmt else {
        panic!("expected query statement");
    };

    let SetExpr::Select(select) = *q.body else {
        panic!("expected SELECT");
    };
    let from = select.from.first().expect("expected FROM");
    let TableFactor::Table { args, .. } = &from.relation else {
        panic!("expected table factor");
    };
    args.as_ref().expect("expected function args").clone()
}

#[tokio::test]
async fn extended_query_parse_rejects_invalid_sql() {
    let parser = TipgQueryParser::new();
    let err = parser.parse_sql("SELCT 1", &[]).await.unwrap_err();

    match err {
        PgWireError::UserError(info) => {
            assert_eq!(info.code, "42601");
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn extended_query_parse_allows_executor_handled_ddl() {
    // `CREATE DATABASE` is handled via the executor's string-based path (not sqlparser-rs).
    let parser = TipgQueryParser::new();
    parser
        .parse_sql("CREATE DATABASE test_db", &[])
        .await
        .expect("CREATE DATABASE should be accepted at Parse");
}

fn encode_value_to_string(value: &Value, col_type: Option<&DataType>) -> String {
    let fields = vec![FieldInfo::new(
        "col".to_string(),
        None,
        None,
        Type::TEXT,
        FieldFormat::Text,
    )];
    let fields = Arc::new(fields);
    let mut encoder = DataRowEncoder::new(fields);
    let tz = crate::types::timestamp::TimeZoneSpec::parse("UTC");
    encode_value(&mut encoder, value, col_type, tz).unwrap();
    let row = encoder.finish().unwrap();

    assert_eq!(row.field_count, 1);
    let mut data = row.data.clone();
    let len = data.get_i32();
    assert!(len >= 0);
    let bytes = data.copy_to_bytes(len as usize);
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[test]
fn test_sqlstate_for_executor_error() {
    // Test legacy InFailedSqlTransaction
    let failed = anyhow::Error::new(InFailedSqlTransaction);
    assert_eq!(sqlstate_for_executor_error(&failed), "25P02");

    // Test generic error (default)
    let other = anyhow::anyhow!("boom");
    assert_eq!(sqlstate_for_executor_error(&other), "XX000");

    // Test string-match fallback: invalid input syntax
    let invalid_syntax = anyhow::anyhow!("invalid input syntax for type integer: \"abc\"");
    assert_eq!(sqlstate_for_executor_error(&invalid_syntax), "22P02");

    // Test string-match fallback: column not found
    let col_not_found = anyhow::anyhow!("column \"age\" does not exist");
    assert_eq!(sqlstate_for_executor_error(&col_not_found), "42703");

    // Test string-match fallback: ambiguous column
    let ambiguous = anyhow::anyhow!("column reference \"id\" is ambiguous");
    assert_eq!(sqlstate_for_executor_error(&ambiguous), "42702");

    // Test string-match fallback: relation not found
    let rel_not_found = anyhow::anyhow!("relation \"users\" does not exist");
    assert_eq!(sqlstate_for_executor_error(&rel_not_found), "42P01");

    // Test string-match fallback: unique constraint violation
    let unique_violation =
        anyhow::anyhow!("duplicate key value violates unique constraint \"pk_users\"");
    assert_eq!(sqlstate_for_executor_error(&unique_violation), "23505");

    // Test string-match fallback: not-null constraint violation
    let not_null = anyhow::anyhow!("violates not-null constraint on column \"email\"");
    assert_eq!(sqlstate_for_executor_error(&not_null), "23502");

    // Test string-match fallback: check constraint violation
    let check = anyhow::anyhow!("violates check constraint \"age_positive\"");
    assert_eq!(sqlstate_for_executor_error(&check), "23514");

    // Test string-match fallback: division by zero
    let div_zero = anyhow::anyhow!("Division by zero");
    assert_eq!(sqlstate_for_executor_error(&div_zero), "22012");

    // Test string-match fallback: permission denied
    let perm_denied = anyhow::anyhow!("permission denied for table users");
    assert_eq!(sqlstate_for_executor_error(&perm_denied), "42501");

    // Test string-match fallback: function not found
    let func_not_found = anyhow::anyhow!("function my_func does not exist");
    assert_eq!(sqlstate_for_executor_error(&func_not_found), "42883");
}

#[test]
fn test_update_tx_status_after_execution_clears_error_on_rollback_to_savepoint() {
    let status = TransactionStatus::Error;
    let tag = Tag::new("ROLLBACK");
    assert_eq!(
        update_tx_status_after_execution(status, &tag),
        TransactionStatus::Transaction
    );
}

#[test]
fn test_update_tx_status_after_execution_keeps_status_for_other_commands() {
    let status = TransactionStatus::Error;
    let tag = Tag::new("SET");
    assert_eq!(
        update_tx_status_after_execution(status, &tag),
        TransactionStatus::Error
    );
}

#[derive(Debug)]
struct TestClient {
    inner: DefaultClient<String>,
    sent: Vec<PgWireBackendMessage>,
}

impl TestClient {
    fn new() -> Self {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        Self {
            inner: DefaultClient::new(addr, false),
            sent: Vec::new(),
        }
    }
}

impl ClientInfo for TestClient {
    fn socket_addr(&self) -> SocketAddr {
        self.inner.socket_addr
    }

    fn is_secure(&self) -> bool {
        self.inner.is_secure
    }

    fn state(&self) -> PgWireConnectionState {
        self.inner.state
    }

    fn set_state(&mut self, new_state: PgWireConnectionState) {
        self.inner.state = new_state;
    }

    fn transaction_status(&self) -> TransactionStatus {
        self.inner.transaction_status
    }

    fn set_transaction_status(&mut self, new_status: TransactionStatus) {
        self.inner.transaction_status = new_status;
    }

    fn metadata(&self) -> &HashMap<String, String> {
        &self.inner.metadata
    }

    fn metadata_mut(&mut self) -> &mut HashMap<String, String> {
        &mut self.inner.metadata
    }
}

impl ClientPortalStore for TestClient {
    type PortalStore = pgwire::api::store::MemPortalStore<String>;

    fn portal_store(&self) -> &Self::PortalStore {
        &self.inner.portal_store
    }
}

impl Sink<PgWireBackendMessage> for TestClient {
    type Error = PgWireError;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: PgWireBackendMessage) -> Result<(), Self::Error> {
        self.get_mut().sent.push(item);
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

#[derive(Debug)]
struct StubExtendedQueryHandler {
    query_parser: Arc<NoopQueryParser>,
    rows: usize,
}

impl StubExtendedQueryHandler {
    fn new() -> Self {
        Self::new_with_rows(5)
    }

    fn new_with_rows(rows: usize) -> Self {
        Self {
            query_parser: Arc::new(NoopQueryParser::new()),
            rows,
        }
    }

    fn select_range_response(rows: usize) -> Response<'static> {
        let fields = Arc::new(vec![FieldInfo::new(
            "n".to_owned(),
            None,
            None,
            Type::INT4,
            FieldFormat::Text,
        )]);

        let row_fields = fields.clone();
        let row_stream = stream::iter(0..rows).map(move |v| {
            let mut encoder = DataRowEncoder::new(row_fields.clone());
            let value = i32::try_from(v).unwrap_or(i32::MAX);
            encoder.encode_field(&value)?;
            encoder.finish()
        });

        Response::Query(QueryResponse::new(fields, row_stream))
    }
}

#[async_trait]
impl ExtendedQueryHandler for StubExtendedQueryHandler {
    type Statement = String;
    type QueryParser = NoopQueryParser;

    fn query_parser(&self) -> Arc<Self::QueryParser> {
        self.query_parser.clone()
    }

    async fn do_query<'a, 'b: 'a, C>(
        &'b self,
        _client: &mut C,
        _portal: &'a Portal<Self::Statement>,
        _max_rows: usize,
    ) -> PgWireResult<Response<'a>>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        Ok(Self::select_range_response(self.rows))
    }

    async fn do_describe_statement<C>(
        &self,
        _client: &mut C,
        _target: &StoredStatement<Self::Statement>,
    ) -> PgWireResult<DescribeStatementResponse>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: PortalStore<Statement = Self::Statement>,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        Ok(DescribeStatementResponse::new(vec![], vec![]))
    }

    async fn do_describe_portal<C>(
        &self,
        _client: &mut C,
        _target: &Portal<Self::Statement>,
    ) -> PgWireResult<DescribePortalResponse>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: PortalStore<Statement = Self::Statement>,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        Ok(DescribePortalResponse::new(vec![]))
    }
}

fn decode_single_text_field(row: &DataRow) -> String {
    let mut data = row.data.clone();
    let len = data.get_i32();
    assert!(len >= 0);
    let bytes = data.copy_to_bytes(len as usize);
    String::from_utf8(bytes.to_vec()).expect("utf8")
}

#[tokio::test]
async fn execute_honors_max_rows_and_suspends_portal() {
    let handler = StubExtendedQueryHandler::new();
    let suspended = Mutex::new(HashMap::<String, SuspendedPortalState>::new());

    let statement = Arc::new(StoredStatement::new(
        "stmt".to_owned(),
        "SELECT 1".to_owned(),
        vec![],
    ));
    let bind = pgwire::messages::extendedquery::Bind::new(
        Some("portal".to_owned()),
        Some("stmt".to_owned()),
        vec![],
        vec![],
        vec![],
    );
    let portal = Portal::try_new(&bind, statement).expect("portal");

    let mut client = TestClient::new();
    client.set_state(PgWireConnectionState::ReadyForQuery);
    client.portal_store().put_portal(Arc::new(portal.clone()));

    on_execute_with_tx_status_fix(
        &handler,
        &suspended,
        &mut client,
        pgwire::messages::extendedquery::Execute::new(Some("portal".to_owned()), 2),
    )
    .await
    .expect("execute 1");

    let msgs = std::mem::take(&mut client.sent);
    assert_eq!(msgs.len(), 3);
    assert!(matches!(msgs[2], PgWireBackendMessage::PortalSuspended(_)));
    assert_eq!(
        msgs.iter()
            .filter_map(|m| match m {
                PgWireBackendMessage::DataRow(r) => Some(decode_single_text_field(r)),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec!["0", "1"]
    );

    on_execute_with_tx_status_fix(
        &handler,
        &suspended,
        &mut client,
        pgwire::messages::extendedquery::Execute::new(Some("portal".to_owned()), 2),
    )
    .await
    .expect("execute 2");

    let msgs = std::mem::take(&mut client.sent);
    assert_eq!(msgs.len(), 3);
    assert!(matches!(msgs[2], PgWireBackendMessage::PortalSuspended(_)));
    assert_eq!(
        msgs.iter()
            .filter_map(|m| match m {
                PgWireBackendMessage::DataRow(r) => Some(decode_single_text_field(r)),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec!["2", "3"]
    );

    on_execute_with_tx_status_fix(
        &handler,
        &suspended,
        &mut client,
        pgwire::messages::extendedquery::Execute::new(Some("portal".to_owned()), 2),
    )
    .await
    .expect("execute 3");

    let msgs = std::mem::take(&mut client.sent);
    assert_eq!(msgs.len(), 2);
    assert!(matches!(msgs[1], PgWireBackendMessage::CommandComplete(_)));
    let PgWireBackendMessage::CommandComplete(complete) = &msgs[1] else {
        panic!("expected CommandComplete");
    };
    assert_eq!(complete.tag, "SELECT 5");
    assert_eq!(
        msgs.iter()
            .filter_map(|m| match m {
                PgWireBackendMessage::DataRow(r) => Some(decode_single_text_field(r)),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec!["4"]
    );
}

#[tokio::test]
async fn execute_errors_when_suspended_portal_count_exceeds_limit() {
    let max_suspended = max_suspended_portals();
    let handler = StubExtendedQueryHandler::new_with_rows(2);
    let suspended = Mutex::new(HashMap::<String, SuspendedPortalState>::new());

    let statement = Arc::new(StoredStatement::new(
        "stmt".to_owned(),
        "SELECT 1".to_owned(),
        vec![],
    ));

    let mut client = TestClient::new();
    client.set_state(PgWireConnectionState::ReadyForQuery);

    for i in 0..max_suspended {
        let portal_name = format!("portal_{i}");
        let bind = pgwire::messages::extendedquery::Bind::new(
            Some(portal_name.clone()),
            Some("stmt".to_owned()),
            vec![],
            vec![],
            vec![],
        );
        let portal = Portal::try_new(&bind, statement.clone()).expect("portal");
        client.portal_store().put_portal(Arc::new(portal));

        on_execute_with_tx_status_fix(
            &handler,
            &suspended,
            &mut client,
            pgwire::messages::extendedquery::Execute::new(Some(portal_name.clone()), 1),
        )
        .await
        .expect("execute should suspend");

        let msgs = std::mem::take(&mut client.sent);
        assert!(msgs
            .iter()
            .any(|m| matches!(m, PgWireBackendMessage::DataRow(_))));
        assert!(msgs
            .iter()
            .any(|m| matches!(m, PgWireBackendMessage::PortalSuspended(_))));
    }

    assert_eq!(suspended.lock().await.len(), max_suspended);

    let portal_name = "portal_over_limit".to_owned();
    let bind = pgwire::messages::extendedquery::Bind::new(
        Some(portal_name.clone()),
        Some("stmt".to_owned()),
        vec![],
        vec![],
        vec![],
    );
    let portal = Portal::try_new(&bind, statement).expect("portal");
    client.portal_store().put_portal(Arc::new(portal));

    let err = on_execute_with_tx_status_fix(
        &handler,
        &suspended,
        &mut client,
        pgwire::messages::extendedquery::Execute::new(Some(portal_name.clone()), 1),
    )
    .await
    .expect_err("expected suspended portal count limit error");

    match err {
        PgWireError::UserError(info) => {
            assert_eq!(info.code, "54000");
            assert!(info.message.contains("too many suspended portals"));
        }
        other => panic!("expected user error, got {other:?}"),
    }

    // The server should not retain suspended portal rows after failing.
    let guard = suspended.lock().await;
    assert_eq!(guard.len(), max_suspended);
    assert!(!guard.contains_key(&portal_name));
}

#[tokio::test]
async fn execute_max_rows_zero_returns_all_rows() {
    let handler = StubExtendedQueryHandler::new();
    let suspended = Mutex::new(HashMap::<String, SuspendedPortalState>::new());

    let statement = Arc::new(StoredStatement::new(
        "stmt".to_owned(),
        "SELECT 1".to_owned(),
        vec![],
    ));
    let bind = pgwire::messages::extendedquery::Bind::new(
        Some("portal".to_owned()),
        Some("stmt".to_owned()),
        vec![],
        vec![],
        vec![],
    );
    let portal = Portal::try_new(&bind, statement).expect("portal");

    let mut client = TestClient::new();
    client.set_state(PgWireConnectionState::ReadyForQuery);
    client.portal_store().put_portal(Arc::new(portal));

    on_execute_with_tx_status_fix(
        &handler,
        &suspended,
        &mut client,
        pgwire::messages::extendedquery::Execute::new(Some("portal".to_owned()), 0),
    )
    .await
    .expect("execute");

    let msgs = std::mem::take(&mut client.sent);
    assert!(matches!(
        msgs.last(),
        Some(PgWireBackendMessage::CommandComplete(_))
    ));
    assert_eq!(
        msgs.iter()
            .filter_map(|m| match m {
                PgWireBackendMessage::DataRow(r) => Some(decode_single_text_field(r)),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec!["0", "1", "2", "3", "4"]
    );
}

#[tokio::test]
async fn execute_errors_when_suspension_buffer_exceeds_limit() {
    let max_buffered_rows = max_suspended_portal_buffer_rows();
    let max_rows = 2usize;
    let handler = StubExtendedQueryHandler::new_with_rows(max_rows + max_buffered_rows + 1);
    let suspended = Mutex::new(HashMap::<String, SuspendedPortalState>::new());

    let statement = Arc::new(StoredStatement::new(
        "stmt".to_owned(),
        "SELECT 1".to_owned(),
        vec![],
    ));
    let bind = pgwire::messages::extendedquery::Bind::new(
        Some("portal".to_owned()),
        Some("stmt".to_owned()),
        vec![],
        vec![],
        vec![],
    );
    let portal = Portal::try_new(&bind, statement).expect("portal");

    let mut client = TestClient::new();
    client.set_state(PgWireConnectionState::ReadyForQuery);
    client.portal_store().put_portal(Arc::new(portal));

    let err = on_execute_with_tx_status_fix(
        &handler,
        &suspended,
        &mut client,
        pgwire::messages::extendedquery::Execute::new(Some("portal".to_owned()), max_rows as i32),
    )
    .await
    .expect_err("expected buffer limit error");

    match err {
        PgWireError::UserError(info) => {
            assert!(info.message.contains("portal suspension buffer exceeded"));
        }
        other => panic!("expected user error, got {other:?}"),
    }

    // The server should not retain suspended portal rows after failing.
    assert!(suspended.lock().await.is_empty());

    let data_rows = client
        .sent
        .iter()
        .filter(|m| matches!(m, PgWireBackendMessage::DataRow(_)))
        .count();
    assert_eq!(data_rows, max_rows);
    assert!(!client
        .sent
        .iter()
        .any(|m| matches!(m, PgWireBackendMessage::PortalSuspended(_))));
    assert!(!client
        .sent
        .iter()
        .any(|m| matches!(m, PgWireBackendMessage::CommandComplete(_))));
}

#[test]
fn test_infer_wildcard_multiway_natural_join_dedups_columns() {
    let stmts =
        crate::sql::parse_sql("SELECT * FROM a NATURAL JOIN b NATURAL JOIN c").expect("parse");
    let stmt = stmts.first().expect("stmt");
    let Statement::Query(query) = stmt else {
        panic!("expected query");
    };
    let SetExpr::Select(select) = query.body.as_ref() else {
        panic!("expected select");
    };

    let schema_a = test_schema(
        "a",
        vec![
            test_column("id", DataType::Int32),
            test_column("a1", DataType::Text),
        ],
    );
    let schema_b = test_schema("b", vec![test_column("b1", DataType::Text)]);
    let schema_c = test_schema(
        "c",
        vec![
            test_column("id", DataType::Int32),
            test_column("c1", DataType::Text),
        ],
    );
    let sources = vec![
        SourceSchema {
            alias: "a".to_string(),
            schema: schema_a,
        },
        SourceSchema {
            alias: "b".to_string(),
            schema: schema_b,
        },
        SourceSchema {
            alias: "c".to_string(),
            schema: schema_c,
        },
    ];

    let schema_refs: Vec<&TableSchema> = sources.iter().map(|s| &s.schema).collect();
    let plan = crate::sql::wildcard::build_join_wildcard_plan(select, &schema_refs).expect("plan");
    assert!(plan.any_merge);
    let names: Vec<String> = plan.columns.into_iter().map(|c| c.name).collect();
    assert_eq!(names, vec!["id", "a1", "b1", "c1"]);
}

#[test]
fn test_infer_wildcard_multiway_using_join_dedups_columns() {
    let stmts = crate::sql::parse_sql("SELECT * FROM a JOIN b USING (id) JOIN c USING (id)")
        .expect("parse");
    let stmt = stmts.first().expect("stmt");
    let Statement::Query(query) = stmt else {
        panic!("expected query");
    };
    let SetExpr::Select(select) = query.body.as_ref() else {
        panic!("expected select");
    };

    let schema_a = test_schema(
        "a",
        vec![
            test_column("id", DataType::Int32),
            test_column("a1", DataType::Text),
        ],
    );
    let schema_b = test_schema(
        "b",
        vec![
            test_column("id", DataType::Int32),
            test_column("b1", DataType::Text),
        ],
    );
    let schema_c = test_schema(
        "c",
        vec![
            test_column("id", DataType::Int32),
            test_column("c1", DataType::Text),
        ],
    );
    let sources = vec![
        SourceSchema {
            alias: "a".to_string(),
            schema: schema_a,
        },
        SourceSchema {
            alias: "b".to_string(),
            schema: schema_b,
        },
        SourceSchema {
            alias: "c".to_string(),
            schema: schema_c,
        },
    ];

    let schema_refs: Vec<&TableSchema> = sources.iter().map(|s| &s.schema).collect();
    let plan = crate::sql::wildcard::build_join_wildcard_plan(select, &schema_refs).expect("plan");
    assert!(plan.any_merge);
    let names: Vec<String> = plan.columns.into_iter().map(|c| c.name).collect();
    assert_eq!(names, vec!["id", "a1", "b1", "c1"]);
}

#[test]
fn test_infer_wildcard_natural_join_common_cols_is_case_sensitive() {
    let stmts = crate::sql::parse_sql("SELECT * FROM a NATURAL JOIN b").expect("parse");
    let stmt = stmts.first().expect("stmt");
    let Statement::Query(query) = stmt else {
        panic!("expected query");
    };
    let SetExpr::Select(select) = query.body.as_ref() else {
        panic!("expected select");
    };

    let schema_a = test_schema("a", vec![test_column("Foo", DataType::Int32)]);
    let schema_b = test_schema("b", vec![test_column("foo", DataType::Int32)]);
    let sources = vec![
        SourceSchema {
            alias: "a".to_string(),
            schema: schema_a,
        },
        SourceSchema {
            alias: "b".to_string(),
            schema: schema_b,
        },
    ];

    let schema_refs: Vec<&TableSchema> = sources.iter().map(|s| &s.schema).collect();
    let plan = crate::sql::wildcard::build_join_wildcard_plan(select, &schema_refs).expect("plan");
    assert!(!plan.any_merge);
    let names: Vec<String> = plan.columns.into_iter().map(|c| c.name).collect();
    assert_eq!(names, vec!["Foo", "foo"]);
}

#[test]
fn test_parse_tenant_username_dot() {
    let (ks, user) = parse_tenant_username("tenant_a.admin");
    assert_eq!(ks, Some("tipg_tenant_tenant_a".to_string()));
    assert_eq!(user, "admin");
}

#[test]
fn test_parse_tenant_username_colon() {
    let (ks, user) = parse_tenant_username("tenant_b:postgres");
    assert_eq!(ks, Some("tipg_tenant_tenant_b".to_string()));
    assert_eq!(user, "postgres");
}

#[test]
fn test_parse_tenant_username_no_separator() {
    let (ks, user) = parse_tenant_username("admin");
    assert_eq!(ks, None);
    assert_eq!(user, "admin");
}

#[test]
fn test_parse_tenant_username_empty_parts() {
    let (ks, user) = parse_tenant_username(".admin");
    assert_eq!(ks, None);
    assert_eq!(user, ".admin");

    let (ks, user) = parse_tenant_username("tenant.");
    assert_eq!(ks, None);
    assert_eq!(user, "tenant.");
}

#[test]
fn test_parse_tenant_username_multiple_dots() {
    let (ks, user) = parse_tenant_username("prod.tenant_a.admin");
    assert_eq!(ks, Some("tipg_tenant_prod".to_string()));
    assert_eq!(user, "tenant_a.admin");
}

#[test]
fn test_parse_tenant_username_multiple_colons() {
    let (ks, user) = parse_tenant_username("prod:tenant_a:admin");
    assert_eq!(ks, Some("tipg_tenant_prod".to_string()));
    assert_eq!(user, "tenant_a:admin");
}

#[test]
fn test_parse_tenant_username_mixed_separators() {
    let (ks, user) = parse_tenant_username("tenant.user:name");
    assert_eq!(ks, Some("tipg_tenant_tenant".to_string()));
    assert_eq!(user, "user:name");

    let (ks, user) = parse_tenant_username("tenant:user.name");
    assert_eq!(ks, Some("tipg_tenant_tenant:user".to_string()));
    assert_eq!(user, "name");
}

#[test]
fn test_parse_tenant_username_special_chars() {
    let (ks, user) = parse_tenant_username("tenant-1.user_name");
    assert_eq!(ks, Some("tipg_tenant_tenant-1".to_string()));
    assert_eq!(user, "user_name");

    let (ks, user) = parse_tenant_username("my_tenant:pg-admin");
    assert_eq!(ks, Some("tipg_tenant_my_tenant".to_string()));
    assert_eq!(user, "pg-admin");
}

#[test]
fn test_parse_tenant_username_numbers() {
    let (ks, user) = parse_tenant_username("tenant123.user456");
    assert_eq!(ks, Some("tipg_tenant_tenant123".to_string()));
    assert_eq!(user, "user456");
}

#[test]
fn test_parse_tenant_username_empty_string() {
    let (ks, user) = parse_tenant_username("");
    assert_eq!(ks, None);
    assert_eq!(user, "");
}

#[test]
fn test_parse_tenant_username_only_separator() {
    let (ks, user) = parse_tenant_username(".");
    assert_eq!(ks, None);
    assert_eq!(user, ".");

    let (ks, user) = parse_tenant_username(":");
    assert_eq!(ks, None);
    assert_eq!(user, ":");
}

#[test]
fn test_parse_tenant_username_unicode() {
    let (ks, user) = parse_tenant_username("租户.用户");
    assert_eq!(ks, Some("tipg_tenant_租户".to_string()));
    assert_eq!(user, "用户");
}

#[test]
fn test_parse_tenant_username_whitespace() {
    let (ks, user) = parse_tenant_username("tenant .user");
    assert_eq!(ks, Some("tipg_tenant_tenant ".to_string()));
    assert_eq!(user, "user");

    let (ks, user) = parse_tenant_username("tenant. user");
    assert_eq!(ks, Some("tipg_tenant_tenant".to_string()));
    assert_eq!(user, " user");
}

#[test]
fn test_auth_bootstrap_transport_errors_do_not_authenticate() {
    let err = anyhow::anyhow!("gRPC transport error: connection reset");
    assert!(DynamicPgHandler::ensure_auth_bootstrapped(Err(err)).is_err());

    let err = anyhow::anyhow!("transport error");
    assert!(DynamicPgHandler::ensure_auth_bootstrapped(Err(err)).is_err());
}

#[test]
fn test_parse_tenant_username_long_names() {
    let long_tenant = "a".repeat(100);
    let long_user = "b".repeat(100);
    let input = format!("{}.{}", long_tenant, long_user);
    let (ks, user) = parse_tenant_username(&input);
    assert_eq!(ks, Some(format!("tipg_tenant_{long_tenant}")));
    assert_eq!(user, long_user);
}

#[test]
fn test_parse_copy_command_basic() {
    let result = DynamicPgHandler::parse_copy_command("COPY users (id, name) FROM stdin");
    assert_eq!(
        result,
        Some((
            "users".to_string(),
            vec!["id".to_string(), "name".to_string()]
        ))
    );
}

#[test]
fn test_parse_copy_command_no_columns() {
    let result = DynamicPgHandler::parse_copy_command("COPY users FROM stdin");
    assert_eq!(result, Some(("users".to_string(), vec![])));
}

#[test]
fn test_parse_copy_command_with_public_schema() {
    let result = DynamicPgHandler::parse_copy_command("COPY public.users (id, name) FROM stdin");
    assert_eq!(
        result,
        Some((
            "public.users".to_string(),
            vec!["id".to_string(), "name".to_string()]
        ))
    );
}

#[test]
fn test_parse_copy_command_case_insensitive() {
    let result = DynamicPgHandler::parse_copy_command("copy USERS (ID, NAME) from STDIN");
    assert_eq!(
        result,
        Some((
            "USERS".to_string(),
            vec!["ID".to_string(), "NAME".to_string()]
        ))
    );
}

#[test]
fn test_parse_copy_command_not_copy() {
    assert_eq!(
        DynamicPgHandler::parse_copy_command("SELECT * FROM users"),
        None
    );
    assert_eq!(
        DynamicPgHandler::parse_copy_command("INSERT INTO users VALUES (1)"),
        None
    );
}

#[test]
fn test_parse_copy_command_copy_keyword_inside_string_literal() {
    assert_eq!(
        DynamicPgHandler::parse_copy_command("SELECT 'COPY users FROM stdin' AS s;"),
        None
    );
}

#[test]
fn test_parse_copy_command_copy_keyword_inside_comment() {
    assert_eq!(
        DynamicPgHandler::parse_copy_command("/* COPY users FROM stdin */ SELECT 1;"),
        None
    );
    assert_eq!(
        DynamicPgHandler::parse_copy_command("-- COPY users FROM stdin\nSELECT 1;"),
        None
    );
}

#[test]
fn test_parse_copy_command_copy_to() {
    assert_eq!(
        DynamicPgHandler::parse_copy_command("COPY users TO stdout"),
        None
    );
}

#[test]
fn test_parse_copy_to_command_basic() {
    let result = DynamicPgHandler::parse_copy_to_command("COPY users TO STDOUT").unwrap();
    assert_eq!(result, Some(("users".to_string(), vec![])));
}

#[test]
fn test_parse_copy_to_command_with_columns() {
    let result =
        DynamicPgHandler::parse_copy_to_command("COPY users (id, name) TO STDOUT").unwrap();
    assert_eq!(
        result,
        Some((
            "users".to_string(),
            vec!["id".to_string(), "name".to_string()]
        ))
    );
}

#[test]
fn test_parse_copy_to_command_with_schema() {
    let result = DynamicPgHandler::parse_copy_to_command("COPY myschema.users TO STDOUT").unwrap();
    assert_eq!(result, Some(("myschema.users".to_string(), vec![])));
}

#[test]
fn test_parse_copy_to_command_not_stdout() {
    assert_eq!(
        DynamicPgHandler::parse_copy_to_command("COPY users TO '/tmp/file'").unwrap(),
        None,
    );
}

#[test]
fn test_parse_copy_to_command_from_stdin() {
    assert_eq!(
        DynamicPgHandler::parse_copy_to_command("COPY users FROM stdin").unwrap(),
        None,
    );
}

#[test]
fn test_parse_copy_to_command_leading_comments() {
    assert_eq!(
        DynamicPgHandler::parse_copy_to_command("-- comment\nCOPY users TO STDOUT").unwrap(),
        Some(("users".to_string(), vec![]))
    );
    assert_eq!(
        DynamicPgHandler::parse_copy_to_command("/* comment */ COPY users TO STDOUT").unwrap(),
        Some(("users".to_string(), vec![]))
    );
}

#[test]
fn test_parse_copy_to_command_rejects_options() {
    let err = DynamicPgHandler::parse_copy_to_command("COPY users TO STDOUT WITH (FORMAT csv)")
        .unwrap_err();
    assert_eq!(err.code, "0A000");
}

#[test]
fn test_parse_copy_to_command_rejects_quoted_identifiers() {
    let err = DynamicPgHandler::parse_copy_to_command("COPY \"users\" TO STDOUT").unwrap_err();
    assert_eq!(err.code, "0A000");
}

#[test]
fn test_parse_copy_command_many_columns() {
    let result = DynamicPgHandler::parse_copy_command(
        "COPY orders (id, user_id, product, quantity, price, created_at) FROM stdin",
    );
    assert_eq!(
        result,
        Some((
            "orders".to_string(),
            vec![
                "id".to_string(),
                "user_id".to_string(),
                "product".to_string(),
                "quantity".to_string(),
                "price".to_string(),
                "created_at".to_string()
            ]
        ))
    );
}

#[test]
fn test_replace_placeholders_basic() {
    assert_eq!(
        replace_placeholders_for_inference("SELECT * FROM users WHERE id = $1"),
        "SELECT * FROM users WHERE id = 1"
    );
    assert_eq!(
        replace_placeholders_for_inference("SELECT * FROM users WHERE id = $1 AND name = $2"),
        "SELECT * FROM users WHERE id = 1 AND name = 1"
    );
}

#[test]
fn test_replace_placeholders_preserves_string_literals() {
    assert_eq!(
        replace_placeholders_for_inference(
            "SELECT * FROM users WHERE email = '$100bill@example.com'"
        ),
        "SELECT * FROM users WHERE email = '$100bill@example.com'"
    );
    assert_eq!(
        replace_placeholders_for_inference("SELECT '${10}' AS template"),
        "SELECT '${10}' AS template"
    );
    assert_eq!(
        replace_placeholders_for_inference(
            "SELECT * FROM t WHERE a = $1 AND b = 'contains $2 inside'"
        ),
        "SELECT * FROM t WHERE a = 1 AND b = 'contains $2 inside'"
    );
}

#[test]
fn test_replace_placeholders_preserves_double_quoted_identifiers() {
    assert_eq!(
        replace_placeholders_for_inference(r#"SELECT * FROM "table$1" WHERE id = $1"#),
        r#"SELECT * FROM "table$1" WHERE id = 1"#
    );
}

#[test]
fn test_replace_placeholders_handles_escaped_single_quotes() {
    assert_eq!(
        replace_placeholders_for_inference("SELECT 'it''s $1' AS msg, $1 AS v"),
        "SELECT 'it''s $1' AS msg, 1 AS v"
    );
}

#[test]
fn test_replace_placeholders_preserves_dollar_quoted_strings() {
    assert_eq!(
        replace_placeholders_for_inference("SELECT $$ $1 $$ AS body, $1 AS v"),
        "SELECT $$ $1 $$ AS body, 1 AS v"
    );
    assert_eq!(
        replace_placeholders_for_inference("SELECT $tag$ $1 $tag$ AS body, $1 AS v"),
        "SELECT $tag$ $1 $tag$ AS body, 1 AS v"
    );
}

#[test]
fn test_replace_placeholders_high_numbers() {
    assert_eq!(
        replace_placeholders_for_inference("SELECT $1, $10, $100, $999"),
        "SELECT 1, 1, 1, 1"
    );
}

#[test]
fn test_count_sql_parameters_ignores_dollar_quoted_strings() {
    assert_eq!(count_sql_parameters("SELECT $$ $99 $$, $1;"), 1);
    assert_eq!(count_sql_parameters("SELECT $tag$ $2 $tag$, $1;"), 1);
    assert_eq!(count_sql_parameters("SELECT $$ $100 $$, $2;"), 2);
    assert_eq!(count_sql_parameters("SELECT 'it''s $10', $2;"), 2);
    assert_eq!(count_sql_parameters(r#"SELECT "table$5", $1;"#), 1);
    assert_eq!(count_sql_parameters("SELECT $1, $10;"), 10);
}

#[test]
fn test_count_sql_parameters_ignores_comments_and_identifier_tokens() {
    assert_eq!(count_sql_parameters("SELECT 1 /* $10 */;"), 0);
    assert_eq!(count_sql_parameters("SELECT 1 -- $10\n;"), 0);
    assert_eq!(count_sql_parameters("SELECT a$1 FROM t WHERE id = $1;"), 1);
    assert_eq!(count_sql_parameters("SELECT 1 /* $10 */ , $2;"), 2);
    assert_eq!(
        count_sql_parameters("SELECT /* outer /* $10 */ inner */ $1;"),
        1
    );
}

#[test]
fn test_find_keyword_outside_strings_ignores_dollar_quoted_strings() {
    let query = "INSERT INTO t VALUES (1) $$ RETURNING $$ RETURNING id";
    let pos = find_keyword_outside_strings(query, "RETURNING").unwrap();
    assert_eq!(pos, query.rfind("RETURNING").unwrap());

    let query = "SELECT $tag$RETURNING$tag$ RETURNING";
    let pos = find_keyword_outside_strings(query, "RETURNING").unwrap();
    assert_eq!(pos, query.rfind("RETURNING").unwrap());

    let query = "SELECT RETURNINGX RETURNING";
    let pos = find_keyword_outside_strings(query, "RETURNING").unwrap();
    assert_eq!(pos, query.rfind("RETURNING").unwrap());
}

#[test]
fn test_substitute_placeholders_preserves_dollar_quoted_strings() {
    let values = vec!["111".to_string()];
    assert_eq!(
        substitute_placeholders_outside_strings_and_dollar(
            "SELECT $$ $1 $$ AS body, $1 AS v",
            &values
        ),
        "SELECT $$ $1 $$ AS body, 111 AS v"
    );

    assert_eq!(
        substitute_placeholders_outside_strings_and_dollar("SELECT 'it''s $1' AS msg, $1", &values),
        "SELECT 'it''s $1' AS msg, 111"
    );
}

#[test]
fn test_substitute_placeholders_handles_multi_digit_numbers() {
    let values = (1..=10).map(|i| i.to_string()).collect::<Vec<_>>();
    assert_eq!(
        substitute_placeholders_outside_strings_and_dollar("SELECT $10, $1", &values),
        "SELECT 10, 1"
    );

    assert_eq!(
        substitute_placeholders_outside_strings_and_dollar("SELECT '${10}', $1", &values),
        "SELECT '${10}', 1"
    );

    assert_eq!(
        substitute_placeholders_outside_strings_and_dollar("SELECT $$ $10 $$, $10", &values),
        "SELECT $$ $10 $$, 10"
    );
}

#[test]
fn test_substitute_placeholders_ignores_comments_and_identifier_tokens() {
    let values = vec!["42".to_string()];
    assert_eq!(
        substitute_placeholders_outside_strings_and_dollar("SELECT 1 /* $1 */ , $1;", &values),
        "SELECT 1 /* $1 */ , 42;"
    );
    assert_eq!(
        substitute_placeholders_outside_strings_and_dollar("SELECT 1 -- $1\n, $1;", &values),
        "SELECT 1 -- $1\n, 42;"
    );
    assert_eq!(
        substitute_placeholders_outside_strings_and_dollar(
            "SELECT a$1 FROM t WHERE id = $1;",
            &values
        ),
        "SELECT a$1 FROM t WHERE id = 42;"
    );
    assert_eq!(
        substitute_placeholders_outside_strings_and_dollar(
            "SELECT /* outer /* $1 */ inner */ $1;",
            &values
        ),
        "SELECT /* outer /* $1 */ inner */ 42;"
    );
}

#[test]
fn test_result_to_response_transaction_start_is_not_empty_query() {
    let resp = result_to_response(ExecuteResult::TransactionStart { tag: "BEGIN" }).unwrap();
    match resp {
        Response::TransactionStart(tag) => {
            let complete = CommandComplete::from(tag);
            assert_eq!(complete.tag, "BEGIN");
        }
        _ => panic!("expected TransactionStart"),
    }
}

#[test]
fn test_result_to_response_transaction_end_is_not_empty_query() {
    let resp = result_to_response(ExecuteResult::TransactionEnd { tag: "COMMIT" }).unwrap();
    match resp {
        Response::TransactionEnd(tag) => {
            let complete = CommandComplete::from(tag);
            assert_eq!(complete.tag, "COMMIT");
        }
        _ => panic!("expected TransactionEnd"),
    }
}

#[test]
fn test_result_to_response_command_complete_is_execution() {
    let resp = result_to_response(ExecuteResult::CommandComplete { tag: "SET" }).unwrap();
    match resp {
        Response::Execution(tag) => {
            let complete = CommandComplete::from(tag);
            assert_eq!(complete.tag, "SET");
        }
        _ => panic!("expected Execution"),
    }
}

#[test]
fn test_result_to_response_empty_is_empty_query() {
    let resp = result_to_response(ExecuteResult::Empty).unwrap();
    assert!(matches!(resp, Response::EmptyQuery));
}

#[tokio::test]
async fn test_extended_query_notice_emits_notice_response() {
    let mut client = RecordingSink::default();
    let results = crate::sql::ExecuteResults(vec![
        ExecuteResult::Notice {
            message: "table \"flow3_notice_test\" does not exist, skipping".to_string(),
        },
        ExecuteResult::CommandComplete { tag: "DROP TABLE" },
    ]);

    let resp = send_notices_and_get_last_response(&mut client, None, results)
        .await
        .unwrap();

    match resp {
        Response::Execution(tag) => {
            let complete = CommandComplete::from(tag);
            assert_eq!(complete.tag, "DROP TABLE");
        }
        _ => panic!("expected Execution"),
    }

    assert_eq!(client.messages.len(), 1);
    match &client.messages[0] {
        PgWireBackendMessage::NoticeResponse(notice) => {
            assert!(notice
                .fields
                .iter()
                .any(|(code, value)| *code == b'S' && value == "NOTICE"));
            assert!(notice
                .fields
                .iter()
                .any(|(code, value)| *code == b'C' && value == "00000"));
            assert!(notice.fields.iter().any(|(code, value)| {
                *code == b'M' && value.contains("does not exist, skipping")
            }));
        }
        other => panic!("expected NoticeResponse, got {other:?}"),
    }
}

#[tokio::test]
async fn test_extended_query_notice_respects_client_min_messages() {
    let mut client = RecordingSink::default();
    let results = crate::sql::ExecuteResults(vec![
        ExecuteResult::Notice {
            message: "test notice".to_string(),
        },
        ExecuteResult::CommandComplete { tag: "DROP TABLE" },
    ]);

    let _resp =
        send_notices_and_get_last_response(&mut client, Some("warning".to_string()), results)
            .await
            .unwrap();

    assert!(client.messages.is_empty());
}

#[test]
fn test_infer_parameter_types_limit() {
    let types = infer_parameter_types("SELECT * FROM users LIMIT $1", 1);
    assert_eq!(types, vec![Type::INT8]);
}

#[test]
fn test_infer_parameter_types_offset() {
    let types = infer_parameter_types("SELECT * FROM users OFFSET $1", 1);
    assert_eq!(types, vec![Type::INT8]);
}

#[test]
fn test_infer_parameter_types_limit_offset() {
    let types = infer_parameter_types("SELECT * FROM users LIMIT $1 OFFSET $2", 2);
    assert_eq!(types, vec![Type::INT8, Type::INT8]);
}

#[test]
fn test_infer_parameter_types_fetch() {
    let types = infer_parameter_types("SELECT * FROM users FETCH FIRST $1 ROWS ONLY", 1);
    assert_eq!(types, vec![Type::INT8]);

    let types = infer_parameter_types("SELECT * FROM users FETCH NEXT $1 ROWS ONLY", 1);
    assert_eq!(types, vec![Type::INT8]);
}

#[test]
fn test_infer_parameter_types_where_clause_defaults_to_text() {
    let types = infer_parameter_types("SELECT * FROM users WHERE id = $1", 1);
    assert_eq!(types, vec![Type::TEXT]);
}

#[test]
fn test_infer_parameter_types_mixed() {
    let types = infer_parameter_types("SELECT * FROM users WHERE id = $1 LIMIT $2 OFFSET $3", 3);
    assert_eq!(types, vec![Type::TEXT, Type::INT8, Type::INT8]);
}

#[test]
fn test_infer_parameter_types_preserves_string_literals() {
    let types = infer_parameter_types("SELECT 'LIMIT $1' FROM users LIMIT $1", 1);
    assert_eq!(types, vec![Type::INT8]);
}

#[test]
fn test_infer_parameter_types_preserves_dollar_quoted() {
    let types = infer_parameter_types("SELECT $$ LIMIT $1 $$ FROM users LIMIT $1", 1);
    assert_eq!(types, vec![Type::INT8]);
}

#[test]
fn test_infer_parameter_types_case_insensitive() {
    let types = infer_parameter_types("SELECT * FROM users limit $1", 1);
    assert_eq!(types, vec![Type::INT8]);

    let types = infer_parameter_types("SELECT * FROM users Offset $1", 1);
    assert_eq!(types, vec![Type::INT8]);
}

#[test]
fn test_infer_parameter_types_no_params() {
    let types = infer_parameter_types("SELECT * FROM users", 0);
    assert!(types.is_empty());
}

#[test]
fn test_infer_parameter_types_non_ascii_does_not_panic() {
    let types = infer_parameter_types("SELECT 'ııı' FROM users LIMIT $1", 1);
    assert_eq!(types, vec![Type::INT8]);
}

#[test]
fn test_substitute_parameters_text_always_quoted() {
    let stmt = Arc::new(StoredStatement::new(
        "stmt".to_string(),
        "SELECT $1::text".to_string(),
        vec![Type::TEXT],
    ));
    let mut portal: Portal<String> = Portal::default();
    portal.name = "portal".to_string();
    portal.statement = stmt;
    portal.parameter_format = Format::UnifiedText;
    portal.parameters = vec![Some(Bytes::from_static(b"001"))];
    portal.result_column_format = Format::UnifiedText;

    assert_eq!(
        substitute_parameters("SELECT $1::text", &portal).unwrap(),
        "SELECT '001'::text"
    );
}

#[test]
fn test_substitute_parameters_unknown_text_format_always_quoted() {
    let stmt = Arc::new(StoredStatement::new(
        "stmt".to_string(),
        "SELECT $1::text".to_string(),
        vec![],
    ));
    let mut portal: Portal<String> = Portal::default();
    portal.name = "portal".to_string();
    portal.statement = stmt;
    portal.parameter_format = Format::UnifiedText;
    portal.parameters = vec![Some(Bytes::from_static(b"001"))];
    portal.result_column_format = Format::UnifiedText;

    assert_eq!(
        substitute_parameters("SELECT $1::text", &portal).unwrap(),
        "SELECT '001'::text"
    );
}

#[test]
fn test_substitute_parameters_unknown_binary_int8_with_nul_renders_number() {
    let stmt = Arc::new(StoredStatement::new(
        "stmt".to_string(),
        "SELECT $1".to_string(),
        vec![],
    ));
    let mut portal: Portal<String> = Portal::default();
    portal.name = "portal".to_string();
    portal.statement = stmt;
    portal.parameter_format = Format::UnifiedBinary;
    portal.parameters = vec![Some(Bytes::copy_from_slice(&1i64.to_be_bytes()))];
    portal.result_column_format = Format::UnifiedText;

    assert_eq!(
        substitute_parameters("SELECT $1", &portal).unwrap(),
        "SELECT 1"
    );
}

#[test]
fn test_substitute_parameters_escapes_single_quotes() {
    let stmt = Arc::new(StoredStatement::new(
        "stmt".to_string(),
        "SELECT $1".to_string(),
        vec![Type::TEXT],
    ));
    let mut portal: Portal<String> = Portal::default();
    portal.name = "portal".to_string();
    portal.statement = stmt;
    portal.parameter_format = Format::UnifiedText;
    portal.parameters = vec![Some(Bytes::from_static(b"O'Reilly"))];
    portal.result_column_format = Format::UnifiedText;

    assert_eq!(
        substitute_parameters("SELECT $1", &portal).unwrap(),
        "SELECT 'O''Reilly'"
    );
}

#[test]
fn test_substitute_parameters_int4_text_format_renders_number() {
    let stmt = Arc::new(StoredStatement::new(
        "stmt".to_string(),
        "SELECT $1".to_string(),
        vec![Type::INT4],
    ));
    let mut portal: Portal<String> = Portal::default();
    portal.name = "portal".to_string();
    portal.statement = stmt;
    portal.parameter_format = Format::UnifiedText;
    portal.parameters = vec![Some(Bytes::from_static(b"42"))];
    portal.result_column_format = Format::UnifiedText;

    assert_eq!(
        substitute_parameters("SELECT $1", &portal).unwrap(),
        "SELECT 42"
    );
}

#[test]
fn test_substitute_parameters_int4_text_format_invalid_errors() {
    let stmt = Arc::new(StoredStatement::new(
        "stmt".to_string(),
        "SELECT $1".to_string(),
        vec![Type::INT4],
    ));
    let mut portal: Portal<String> = Portal::default();
    portal.name = "portal".to_string();
    portal.statement = stmt;
    portal.parameter_format = Format::UnifiedText;
    portal.parameters = vec![Some(Bytes::from_static(b"not-a-number"))];
    portal.result_column_format = Format::UnifiedText;

    assert!(substitute_parameters("SELECT $1", &portal).is_err());
}

#[test]
fn test_substitute_parameters_uuid_binary_format_renders_uuid_literal() {
    let stmt = Arc::new(StoredStatement::new(
        "stmt".to_string(),
        "SELECT $1".to_string(),
        vec![Type::UUID],
    ));
    let uuid = uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").expect("valid uuid");
    let mut portal: Portal<String> = Portal::default();
    portal.name = "portal".to_string();
    portal.statement = stmt;
    portal.parameter_format = Format::UnifiedBinary;
    portal.parameters = vec![Some(Bytes::copy_from_slice(uuid.as_bytes()))];
    portal.result_column_format = Format::UnifiedText;

    assert_eq!(
        substitute_parameters("SELECT $1", &portal).unwrap(),
        "SELECT '550e8400-e29b-41d4-a716-446655440000'::uuid"
    );
}

#[test]
fn test_encode_value_timestamp_negative_millis() {
    let col_type = DataType::Timestamp;
    assert_eq!(
        encode_value_to_string(&Value::Timestamp(-1), Some(&col_type)),
        "1969-12-31 23:59:59.999000"
    );
    assert_eq!(
        encode_value_to_string(&Value::Timestamp(-1001), Some(&col_type)),
        "1969-12-31 23:59:58.999000"
    );
}

#[test]
fn test_encode_value_timestamp_year_0001() {
    use chrono::{TimeZone, Utc};

    let col_type = DataType::Timestamp;
    let ts = Utc
        .with_ymd_and_hms(1, 1, 1, 0, 0, 2)
        .single()
        .unwrap()
        .timestamp_millis();
    assert_eq!(
        encode_value_to_string(&Value::Timestamp(ts), Some(&col_type)),
        "0001-01-01 00:00:02"
    );
}

#[test]
fn test_encode_value_int64_as_timestamp_negative_millis() {
    let col_type = DataType::Timestamp;
    assert_eq!(
        encode_value_to_string(&Value::Int64(-1), Some(&col_type)),
        "1969-12-31 23:59:59.999000"
    );
}

#[tokio::test]
async fn infer_fs9_schema_directory_mode() {
    let dir = unique_fs9_infer_base("dir");
    fs::write(dir.join("a.txt"), b"x").expect("write");

    let path = format!("{}/", dir.display());
    let sql = format!("SELECT * FROM extensions.fs9('{path}')");
    let args = extract_fs9_args(&sql);

    let schema = infer_fs9_table_function_schema(&args, true)
        .await
        .expect("schema");
    let cols: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();
    assert_eq!(cols, vec!["path", "type", "size", "mode", "mtime"]);

    cleanup_dir(&dir);
}

#[tokio::test]
async fn infer_fs9_schema_directory_via_stat() {
    let dir = unique_fs9_infer_base("dir-stat");
    fs::create_dir_all(dir.join("nested")).expect("create nested dir");

    let path = dir.to_string_lossy().to_string();
    let sql = format!("SELECT * FROM extensions.fs9('{path}')");
    let args = extract_fs9_args(&sql);

    let schema = infer_fs9_table_function_schema(&args, true)
        .await
        .expect("schema");
    let cols: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();
    assert_eq!(cols, vec!["path", "type", "size", "mode", "mtime"]);

    cleanup_dir(&dir);
}

#[tokio::test]
async fn infer_fs9_schema_csv_headers() {
    let dir = unique_fs9_infer_base("csv");
    let csv_path = dir.join("users.csv");
    fs::write(&csv_path, b"name,age,city\nAlice,30,Beijing\n").expect("write csv");

    let path = csv_path.to_string_lossy().to_string();
    let sql = format!("SELECT * FROM extensions.fs9('{path}')");
    let args = extract_fs9_args(&sql);

    let schema = infer_fs9_table_function_schema(&args, true)
        .await
        .expect("schema");
    let cols: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();
    assert_eq!(cols, vec!["_line_number", "name", "age", "city", "_path"]);
    assert_eq!(schema.columns[0].data_type, DataType::Int64);
    assert_eq!(schema.columns[1].data_type, DataType::Text);

    cleanup_dir(&dir);
}

#[tokio::test]
async fn infer_fs9_schema_jsonl_is_jsonb() {
    let dir = unique_fs9_infer_base("jsonl");
    let jsonl_path = dir.join("logs.jsonl");
    fs::write(&jsonl_path, b"{\"a\":1}\n").expect("write jsonl");

    let path = jsonl_path.to_string_lossy().to_string();
    let sql = format!("SELECT * FROM extensions.fs9('{path}')");
    let args = extract_fs9_args(&sql);

    let schema = infer_fs9_table_function_schema(&args, true)
        .await
        .expect("schema");
    let cols: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();
    assert_eq!(cols, vec!["_line_number", "line", "_path"]);
    assert_eq!(schema.columns[1].data_type, DataType::Jsonb);

    cleanup_dir(&dir);
}

#[tokio::test]
async fn infer_fs9_schema_glob_uses_first_match() {
    let dir = unique_fs9_infer_base("glob");
    fs::write(dir.join("a.csv"), b"foo\n1\n").expect("write a.csv");
    fs::write(dir.join("b.csv"), b"bar\n1\n").expect("write b.csv");

    let pattern = format!("{}/*.csv", dir.display());
    let sql = format!("SELECT * FROM extensions.fs9('{pattern}')");
    let args = extract_fs9_args(&sql);

    let schema = infer_fs9_table_function_schema(&args, true)
        .await
        .expect("schema");
    let cols: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();
    assert_eq!(cols, vec!["_line_number", "foo", "_path"]);

    cleanup_dir(&dir);
}

#[tokio::test]
async fn infer_fs9_schema_non_superuser_is_fallback() {
    let dir = unique_fs9_infer_base("nosu");
    let csv_path = dir.join("users.csv");
    fs::write(&csv_path, b"name\nAlice\n").expect("write csv");

    let path = csv_path.to_string_lossy().to_string();
    let sql = format!("SELECT * FROM extensions.fs9('{path}')");
    let args = extract_fs9_args(&sql);

    let schema = infer_fs9_table_function_schema(&args, false)
        .await
        .expect("schema");
    let cols: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();
    assert_eq!(cols, vec!["_line_number", "line", "_path"]);
    assert_eq!(schema.columns[1].data_type, DataType::Text);

    cleanup_dir(&dir);
}
