use super::*;
use crate::sql::analyzer::catalog::MockCatalog;
use crate::sql::analyzer::{Analyzer, Catalog};
use crate::sql::error::SqlError;
use crate::types::{Row, Value};
use async_trait::async_trait;
use bytes::Buf;
use bytes::Bytes;
use pgwire::api::auth::ServerParameterProvider;
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
use super::params::{count_sql_parameters, decode_parameters};
use super::portal::{
    max_suspended_portal_buffer_rows, max_suspended_portals, on_execute_with_tx_status_fix,
    update_tx_status_after_execution, SuspendedPortalState,
};
use super::prepared::{PreparedExec, PreparedStatement};
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

/// Test-local struct mirroring the deleted type_infer::SourceSchema.
/// Only used by wildcard tests; production code uses `&[&TableSchema]`.
struct SourceSchema {
    alias: String,
    schema: TableSchema,
}

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
        collation: None,
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
    encode_value(&mut encoder, value, col_type, tz, FieldFormat::Text).unwrap();
    let row = encoder.finish().unwrap();

    assert_eq!(row.field_count, 1);
    let mut data = row.data.clone();
    let len = data.get_i32();
    assert!(len >= 0);
    let bytes = data.copy_to_bytes(len as usize);
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn parse_single_statement(sql: &str) -> sqlparser::ast::Statement {
    let mut stmts = crate::sql::parse_sql(sql).expect("SQL parse");
    assert_eq!(stmts.len(), 1, "expected one statement");
    stmts.remove(0)
}

fn analyze_statement_with_params(
    catalog: &dyn Catalog,
    sql: &str,
    param_count: usize,
    client_oids: &[Option<DataType>],
) -> Result<Vec<DataType>, SqlError> {
    let stmt = parse_single_statement(sql);
    let mut analyzer = Analyzer::new_with_params(catalog, param_count, client_oids);
    analyzer.analyze_statement(&stmt).map_err(SqlError::from)?;
    analyzer.finalize_param_types().map_err(SqlError::from)
}

#[test]
fn test_sqlstate_for_executor_error() {
    // SqlError::InFailedTransaction → 25P02
    let failed: anyhow::Error = SqlError::InFailedTransaction.into();
    assert_eq!(sqlstate_for_executor_error(&failed), "25P02");

    // Untyped anyhow → XX000
    let other = anyhow::anyhow!("boom");
    assert_eq!(sqlstate_for_executor_error(&other), "XX000");

    // SqlError downcast path: each variant gets correct SQLSTATE
    let cases: Vec<(SqlError, &str)> = vec![
        (
            SqlError::InvalidInputSyntax {
                type_name: "integer".into(),
                value: "abc".into(),
            },
            "22P02",
        ),
        (
            SqlError::ColumnNotFound {
                column: "age".into(),
                hint: None,
            },
            "42703",
        ),
        (SqlError::AmbiguousColumn("id".into()), "42702"),
        (SqlError::RelationNotFound("users".into()), "42P01"),
        (
            SqlError::UniqueViolation {
                constraint: "pk_users".into(),
                message: "dup".into(),
            },
            "23505",
        ),
        (
            SqlError::NotNullViolation {
                column: "email".into(),
                relation: "users".into(),
                message: "null".into(),
            },
            "23502",
        ),
        (
            SqlError::CheckViolation {
                table: "t".into(),
                constraint: "age_positive".into(),
                detail: String::new(),
            },
            "23514",
        ),
        (SqlError::DivisionByZero, "22012"),
        (
            SqlError::PermissionDenied {
                object_type: "table".into(),
                object_name: "users".into(),
            },
            "42501",
        ),
        (SqlError::FunctionNotFound("my_func".into()), "42883"),
        (SqlError::DuplicateRelation("my_idx".into()), "42P07"),
        (
            SqlError::NumericValueOutOfRange {
                message: "integer out of range".into(),
            },
            "22003",
        ),
        (
            SqlError::StringDataRightTruncation { max_length: 10u64 },
            "22001",
        ),
        (
            SqlError::AmbiguousOperator {
                message: "operator is not unique: unknown + unknown".into(),
            },
            "42725",
        ),
    ];

    for (sql_err, expected_code) in cases {
        let anyhow_err: anyhow::Error = sql_err.into();
        assert_eq!(
            sqlstate_for_executor_error(&anyhow_err),
            expected_code,
            "failed for SQLSTATE {}",
            expected_code
        );
    }
}

#[test]
fn prepared_unknown_plus_unknown_returns_42725() {
    let catalog = MockCatalog::empty();
    let stmt = parse_single_statement("SELECT $1 + $1");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42725");
    assert!(sql.to_string().contains("operator is not unique"));
}

#[test]
fn prepared_unknown_plus_int_succeeds() {
    let catalog = MockCatalog::empty();
    let types = analyze_statement_with_params(&catalog, "SELECT $1 + 1", 1, &[None]).unwrap();
    assert_eq!(types, vec![DataType::Int32]);
}

#[test]
fn prepared_unknown_concat_unknown_resolves_text() {
    let catalog = MockCatalog::empty();
    let types =
        analyze_statement_with_params(&catalog, "SELECT $1 || $2", 2, &[None, None]).unwrap();
    assert_eq!(types, vec![DataType::Text, DataType::Text]);
}

#[test]
fn prepared_pg_typeof_unknown_returns_42p18() {
    let catalog = MockCatalog::empty();
    let err =
        analyze_statement_with_params(&catalog, "SELECT pg_typeof($1)", 1, &[None]).unwrap_err();
    assert_eq!(err.sqlstate(), "42P18");
}

#[test]
fn prepared_explicit_cast_plus_succeeds() {
    let catalog = MockCatalog::empty();
    let types =
        analyze_statement_with_params(&catalog, "SELECT $1::int + $2::int", 2, &[None, None])
            .unwrap();
    assert_eq!(types, vec![DataType::Int32, DataType::Int32]);
}

#[test]
fn prepared_text_column_plus_text_column_stays_42883() {
    let catalog = MockCatalog::builder()
        .table("t", vec![("name", DataType::Text, true)])
        .build();
    let stmt = parse_single_statement("SELECT name + name FROM t");
    let mut analyzer = Analyzer::new_with_params(&catalog, 0, &[]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42883");
}

// ── Mixed-unknown prepared statement tests (PG 42725 parity) ────

#[test]
fn prepared_param_plus_literal_returns_42725() {
    let catalog = MockCatalog::empty();
    let stmt = parse_single_statement("SELECT $1 + '1'");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42725");
    assert!(sql.to_string().contains("operator is not unique"));
}

#[test]
fn prepared_param_minus_literal_returns_42725() {
    let catalog = MockCatalog::empty();
    let stmt = parse_single_statement("SELECT $1 - '1'");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42725");
}

#[test]
fn prepared_param_mod_literal_returns_42725() {
    let catalog = MockCatalog::empty();
    let stmt = parse_single_statement("SELECT $1 % '1'");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42725");
}

#[test]
fn prepared_explicit_cast_stays_42883() {
    let catalog = MockCatalog::empty();
    let stmt = parse_single_statement("SELECT $1 + '1'::text");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42883");
}

#[test]
fn prepared_param_plus_null_returns_42725() {
    let catalog = MockCatalog::empty();
    let stmt = parse_single_statement("SELECT $1 + NULL");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42725");
}

// ── Extended 42725 SqlError conversion coverage (#911) ──

#[test]
fn prepared_unknown_minus_unknown_returns_42725() {
    let catalog = MockCatalog::empty();
    let err =
        analyze_statement_with_params(&catalog, "SELECT $1 - $2", 2, &[None, None]).unwrap_err();
    assert_eq!(err.sqlstate(), "42725");
}

#[test]
fn prepared_unknown_bitand_unknown_returns_42725() {
    let catalog = MockCatalog::empty();
    let err =
        analyze_statement_with_params(&catalog, "SELECT $1 & $2", 2, &[None, None]).unwrap_err();
    assert_eq!(err.sqlstate(), "42725");
}

#[test]
fn prepared_unknown_shl_unknown_returns_42725() {
    let catalog = MockCatalog::empty();
    let err =
        analyze_statement_with_params(&catalog, "SELECT $1 << $2", 2, &[None, None]).unwrap_err();
    assert_eq!(err.sqlstate(), "42725");
}

#[test]
fn prepared_unknown_exp_unknown_returns_42725() {
    let catalog = MockCatalog::empty();
    let err =
        analyze_statement_with_params(&catalog, "SELECT $1 ^ $2", 2, &[None, None]).unwrap_err();
    assert_eq!(err.sqlstate(), "42725");
}

#[test]
fn prepared_param_bitand_literal_returns_42725() {
    let catalog = MockCatalog::empty();
    let stmt = parse_single_statement("SELECT $1 & '1'");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42725");
}

#[test]
fn prepared_param_exp_literal_returns_42725() {
    let catalog = MockCatalog::empty();
    let stmt = parse_single_statement("SELECT $1 ^ '1'");
    let mut analyzer = Analyzer::new_with_params(&catalog, 1, &[None]);
    let err = analyzer.analyze_statement(&stmt).unwrap_err();
    let sql: SqlError = err.into();
    assert_eq!(sql.sqlstate(), "42725");
}

#[test]
fn prepared_int_plus_int_succeeds() {
    let catalog = MockCatalog::empty();
    let types = analyze_statement_with_params(&catalog, "SELECT 1 + 2", 0, &[]).unwrap();
    assert_eq!(types, vec![]);
}

#[test]
fn prepared_literal_concat_literal_succeeds() {
    let catalog = MockCatalog::empty();
    let types =
        analyze_statement_with_params(&catalog, "SELECT 'hello' || 'world'", 0, &[]).unwrap();
    assert_eq!(types, vec![]);
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

#[test]
fn parse_startup_options_supports_quoted_values() {
    assert_eq!(
        parse_startup_options("-c application_name='my app'"),
        vec![("application_name".to_string(), "my app".to_string())]
    );

    assert_eq!(
        parse_startup_options(r#"-c search_path="public, ext""#),
        vec![("search_path".to_string(), "public, ext".to_string())]
    );

    assert_eq!(
        parse_startup_options(r#"-c application_name='my app' -c search_path="public, ext""#),
        vec![
            ("application_name".to_string(), "my app".to_string()),
            ("search_path".to_string(), "public, ext".to_string())
        ]
    );
}

#[test]
fn parse_startup_options_supports_backslash_escaping() {
    // Allow spaces without quotes via backslash-escape.
    assert_eq!(
        parse_startup_options("-c application_name=my\\ app"),
        vec![("application_name".to_string(), "my app".to_string())]
    );
}

#[test]
fn test_parameter_status_includes_common_keys() {
    let mut client = TestClient::new();
    client
        .metadata_mut()
        .insert(METADATA_ACTUAL_USER.to_string(), "admin".to_string());
    client
        .metadata_mut()
        .insert("application_name".to_string(), "pg-tikv-tests".to_string());
    client
        .metadata_mut()
        .insert(METADATA_AUTH_IS_SUPERUSER.to_string(), "on".to_string());

    let params = PgServerParameterProvider
        .server_parameters(&client)
        .expect("server parameters should be provided");

    for key in [
        "server_version",
        "server_version_num",
        "server_encoding",
        "client_encoding",
        "DateStyle",
        "TimeZone",
        "standard_conforming_strings",
        "integer_datetimes",
        "IntervalStyle",
        "application_name",
        "search_path",
        "default_transaction_isolation",
        "is_superuser",
        "session_authorization",
    ] {
        assert!(
            params.contains_key(key),
            "missing ParameterStatus key '{}'",
            key
        );
    }

    assert_eq!(
        params.get("application_name").map(String::as_str),
        Some("pg-tikv-tests")
    );
    assert_eq!(params.get("is_superuser").map(String::as_str), Some("on"));
    assert_eq!(
        params.get("session_authorization").map(String::as_str),
        Some("admin")
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
struct TestPreparedClient {
    inner: DefaultClient<PreparedStatement>,
    sent: Vec<PgWireBackendMessage>,
}

impl TestPreparedClient {
    fn new() -> Self {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        Self {
            inner: DefaultClient::new(addr, false),
            sent: Vec::new(),
        }
    }
}

impl ClientInfo for TestPreparedClient {
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

impl ClientPortalStore for TestPreparedClient {
    type PortalStore = pgwire::api::store::MemPortalStore<PreparedStatement>;

    fn portal_store(&self) -> &Self::PortalStore {
        &self.inner.portal_store
    }
}

impl Sink<PgWireBackendMessage> for TestPreparedClient {
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
    client
        .portal_store()
        .put_portal(Arc::new(portal.clone()))
        .unwrap();

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
        client.portal_store().put_portal(Arc::new(portal)).unwrap();

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
    client.portal_store().put_portal(Arc::new(portal)).unwrap();

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
    client.portal_store().put_portal(Arc::new(portal)).unwrap();

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
    client.portal_store().put_portal(Arc::new(portal)).unwrap();

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
    let (table, cols, _opts) = result.unwrap();
    assert_eq!(table, "users");
    assert!(cols.is_empty());
}

#[test]
fn test_parse_copy_to_command_with_columns() {
    let result =
        DynamicPgHandler::parse_copy_to_command("COPY users (id, name) TO STDOUT").unwrap();
    let (table, cols, _opts) = result.unwrap();
    assert_eq!(table, "users");
    assert_eq!(cols, vec!["id".to_string(), "name".to_string()]);
}

#[test]
fn test_parse_copy_to_command_with_schema() {
    let result = DynamicPgHandler::parse_copy_to_command("COPY myschema.users TO STDOUT").unwrap();
    let (table, cols, _opts) = result.unwrap();
    assert_eq!(table, "myschema.users");
    assert!(cols.is_empty());
}

#[test]
fn test_parse_copy_to_command_not_stdout() {
    assert!(
        DynamicPgHandler::parse_copy_to_command("COPY users TO '/tmp/file'")
            .unwrap()
            .is_none()
    );
}

#[test]
fn test_parse_copy_to_command_from_stdin() {
    assert!(
        DynamicPgHandler::parse_copy_to_command("COPY users FROM stdin")
            .unwrap()
            .is_none()
    );
}

#[test]
fn test_parse_copy_to_command_leading_comments() {
    let result1 =
        DynamicPgHandler::parse_copy_to_command("-- comment\nCOPY users TO STDOUT").unwrap();
    let (t1, c1, _) = result1.unwrap();
    assert_eq!(t1, "users");
    assert!(c1.is_empty());

    let result2 =
        DynamicPgHandler::parse_copy_to_command("/* comment */ COPY users TO STDOUT").unwrap();
    let (t2, c2, _) = result2.unwrap();
    assert_eq!(t2, "users");
    assert!(c2.is_empty());
}

#[test]
fn test_parse_copy_to_command_with_csv_format() {
    use crate::protocol::copy_format::CopyFormat;
    let result =
        DynamicPgHandler::parse_copy_to_command("COPY users TO STDOUT WITH (FORMAT csv)").unwrap();
    let (table, cols, opts) = result.unwrap();
    assert_eq!(table, "users");
    assert!(cols.is_empty());
    assert_eq!(opts.format, CopyFormat::Csv);
    assert_eq!(opts.delimiter, b',');
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
async fn test_result_to_response_with_format_uses_per_column_formats() {
    let resp = result_to_response_with_format(
        ExecuteResult::Select {
            columns: vec!["id".to_string(), "name".to_string()],
            column_types: Some(vec![DataType::Int64, DataType::Text]),
            rows: vec![Row::new(vec![
                Value::Int64(42),
                Value::Text("alice".to_string()),
            ])],
            timezone: Arc::<str>::from("UTC"),
        },
        &Format::Individual(vec![1, 0]),
    )
    .unwrap();

    let Response::Query(query) = resp else {
        panic!("expected query response");
    };

    let schema = query.row_schema();
    assert_eq!(schema[0].format(), FieldFormat::Binary);
    assert_eq!(schema[1].format(), FieldFormat::Text);

    let rows: Vec<_> = query.data_rows().collect().await;
    let row = rows
        .into_iter()
        .next()
        .expect("row exists")
        .expect("row ok");

    let mut data = row.data.clone();
    let len1 = data.get_i32();
    assert_eq!(len1, 8);
    assert_eq!(data.get_i64(), 42);

    let len2 = data.get_i32();
    assert_eq!(len2, 5);
    let name = String::from_utf8(data.copy_to_bytes(len2 as usize).to_vec()).expect("utf8");
    assert_eq!(name, "alice");
}

#[tokio::test]
async fn test_result_to_response_with_format_falls_back_to_text_for_array_binary_request() {
    let resp = result_to_response_with_format(
        ExecuteResult::Select {
            columns: vec!["arr".to_string()],
            column_types: Some(vec![DataType::Array(Box::new(DataType::Int32))]),
            rows: vec![Row::new(vec![Value::Array(vec![
                Value::Int32(1),
                Value::Int32(2),
            ])])],
            timezone: Arc::<str>::from("UTC"),
        },
        &Format::UnifiedBinary,
    )
    .unwrap();

    let Response::Query(query) = resp else {
        panic!("expected query response");
    };

    let schema = query.row_schema();
    assert_eq!(schema[0].datatype(), &Type::INT4_ARRAY);
    assert_eq!(schema[0].format(), FieldFormat::Text);

    let rows: Vec<_> = query.data_rows().collect().await;
    let row = rows
        .into_iter()
        .next()
        .expect("row exists")
        .expect("row ok");
    assert_eq!(decode_single_text_field(&row), "{1,2}");
}

#[tokio::test]
async fn test_result_to_response_does_not_rewrite_question_column_by_value() {
    let resp = result_to_response_with_format(
        ExecuteResult::Select {
            columns: vec!["?column?".to_string()],
            column_types: Some(vec![DataType::Text]),
            rows: vec![Row::new(vec![Value::Text(
                "PostgreSQL 17.7 on x86_64-pc-linux-gnu".to_string(),
            )])],
            timezone: Arc::<str>::from("UTC"),
        },
        &Format::UnifiedText,
    )
    .unwrap();

    let Response::Query(query) = resp else {
        panic!("expected query response");
    };
    let schema = query.row_schema();
    assert_eq!(schema.len(), 1);
    assert_eq!(schema[0].name(), "?column?");
}

#[tokio::test]
async fn test_result_to_response_preserves_explicit_version_column_name() {
    let resp = result_to_response_with_format(
        ExecuteResult::Select {
            columns: vec!["version".to_string()],
            column_types: Some(vec![DataType::Text]),
            rows: vec![Row::new(vec![Value::Text(
                "PostgreSQL 17.7 on x86_64-pc-linux-gnu".to_string(),
            )])],
            timezone: Arc::<str>::from("UTC"),
        },
        &Format::UnifiedText,
    )
    .unwrap();

    let Response::Query(query) = resp else {
        panic!("expected query response");
    };
    let schema = query.row_schema();
    assert_eq!(schema.len(), 1);
    assert_eq!(schema[0].name(), "version");
}

#[tokio::test]
async fn test_extended_query_notice_emits_notice_response() {
    let mut client = RecordingSink::default();
    let results = crate::sql::ExecuteResults(vec![
        ExecuteResult::Notice {
            message: "table \"flow3_notice_test\" does not exist, skipping".to_string(),
            severity: "NOTICE".to_string(),
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
            severity: "NOTICE".to_string(),
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
fn test_client_allows_message_defaults_to_notice_threshold() {
    // PG default client_min_messages is NOTICE.
    assert!(client_allows_message(None, "NOTICE"));
    assert!(client_allows_message(None, "WARNING"));
    assert!(!client_allows_message(None, "INFO"));
}

// ── decode_parameters tests (converted from substitute_parameters) ──────────

/// Helper to create a test PreparedStatement from a SQL string.
fn test_prepared_stmt(sql: &str) -> PreparedStatement {
    PreparedStatement {
        sql: sql.to_string(),
        exec: PreparedExec::RawSqlUtility,
        output_schema: vec![],
        param_data_types: vec![],
        table_versions: vec![],
    }
}

#[tokio::test]
async fn extended_query_bind_parameter_count_mismatch_returns_08p01() {
    let handler = test_dynamic_handler();
    let mut client = TestPreparedClient::new();
    let stmt = Arc::new(StoredStatement::new(
        "stmt".to_string(),
        test_prepared_stmt_analyzed(
            "SELECT $1::int, $2::int",
            vec![],
            vec![DataType::Int32, DataType::Int32],
        ),
        vec![Type::INT4, Type::INT4],
    ));
    client
        .portal_store()
        .put_statement(stmt)
        .expect("store statement");

    let bind = pgwire::messages::extendedquery::Bind::new(
        Some("portal".to_string()),
        Some("stmt".to_string()),
        vec![],
        vec![Some(Bytes::from_static(b"1"))],
        vec![],
    );
    let err = handler
        .on_bind(&mut client, bind)
        .await
        .expect_err("bind should fail");
    match err {
        PgWireError::UserError(info) => {
            assert_eq!(info.code, "08P01");
            assert!(info.message.contains("bind message supplies 1 parameters"));
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

fn test_prepared_stmt_analyzed(
    sql: &str,
    output_schema: Vec<(
        String,
        DataType,
        Option<crate::sql::collation::ResolvedCollation>,
    )>,
    param_data_types: Vec<DataType>,
) -> PreparedStatement {
    let analyzed = crate::sql::analyzer::types::AnalyzedQuery {
        ctes: vec![],
        body: crate::sql::analyzer::types::AnalyzedQueryBody::Values(vec![vec![]]),
        order_by: vec![],
        limit: None,
        offset: None,
        output_schema: output_schema.clone(),
    };
    PreparedStatement {
        sql: sql.to_string(),
        exec: PreparedExec::AnalyzedQuery {
            analyzed,
            locks: vec![],
            select_into: None,
            required_privileges: vec![],
            has_recursive_cte: false,
        },
        output_schema: output_schema
            .iter()
            .map(|(name, dt, _)| (name.clone(), dt.clone()))
            .collect(),
        param_data_types,
        table_versions: vec![],
    }
}

fn test_dynamic_handler() -> DynamicPgHandler {
    let pool = Arc::new(crate::pool::TikvClientPool::new(vec![]));
    let server_config = crate::config::ServerConfig::default().shared();
    DynamicPgHandler::new_with_pool(pool, None, server_config)
}

fn extract_query_schema(resp: Response<'static>) -> Vec<(String, Type)> {
    let Response::Query(query) = resp else {
        panic!("expected query response");
    };
    query
        .row_schema()
        .iter()
        .map(|f| (f.name().to_string(), f.datatype().clone()))
        .collect()
}

async fn assert_describe_execute_metadata_agreement(
    sql: &str,
    output_schema: Vec<(
        String,
        DataType,
        Option<crate::sql::collation::ResolvedCollation>,
    )>,
    param_types: Vec<Type>,
) {
    let handler = test_dynamic_handler();
    let mut client = TestPreparedClient::new();
    let prepared = test_prepared_stmt_analyzed(sql, output_schema.clone(), vec![]);
    let stored = StoredStatement::new("stmt".to_string(), prepared, param_types);

    let describe = handler
        .do_describe_statement(&mut client, &stored)
        .await
        .expect("describe statement");
    let describe_fields: Vec<(String, Type)> = describe
        .fields
        .iter()
        .map(|f| (f.name().to_string(), f.datatype().clone()))
        .collect();

    let exec_columns: Vec<String> = output_schema.iter().map(|(n, _, _)| n.clone()).collect();
    let exec_types: Vec<DataType> = output_schema.iter().map(|(_, t, _)| t.clone()).collect();
    let execute_resp = result_to_response_with_format(
        ExecuteResult::Select {
            columns: exec_columns,
            column_types: Some(exec_types),
            rows: vec![],
            timezone: Arc::<str>::from("UTC"),
        },
        &Format::UnifiedText,
    )
    .expect("encode execute response");
    let execute_fields = extract_query_schema(execute_resp);
    assert_eq!(describe_fields, execute_fields);
}

#[test]
fn test_decode_parameters_text_always_text_value() {
    let stmt = Arc::new(StoredStatement::new(
        "stmt".to_string(),
        test_prepared_stmt("SELECT $1::text"),
        vec![Type::TEXT],
    ));
    let mut portal: Portal<PreparedStatement> = Portal::default();
    portal.name = "portal".to_string();
    portal.statement = stmt;
    portal.parameter_format = Format::UnifiedText;
    portal.parameters = vec![Some(Bytes::from_static(b"001"))];
    portal.result_column_format = Format::UnifiedText;

    let values = decode_parameters(&portal).unwrap();
    assert_eq!(values, vec![Some(Value::Text("001".to_string()))]);
}

#[test]
fn test_decode_parameters_unknown_text_format_text_value() {
    // Empty parameter_types defaults to TEXT in decode_parameters
    let stmt = Arc::new(StoredStatement::new(
        "stmt".to_string(),
        test_prepared_stmt("SELECT $1::text"),
        vec![],
    ));
    let mut portal: Portal<PreparedStatement> = Portal::default();
    portal.name = "portal".to_string();
    portal.statement = stmt;
    portal.parameter_format = Format::UnifiedText;
    portal.parameters = vec![Some(Bytes::from_static(b"001"))];
    portal.result_column_format = Format::UnifiedText;

    let values = decode_parameters(&portal).unwrap();
    assert_eq!(values, vec![Some(Value::Text("001".to_string()))]);
}

#[test]
fn test_decode_parameters_int8_binary_decodes_correctly() {
    // Explicit INT8 type — intent is "binary int8 decoding works"
    let stmt = Arc::new(StoredStatement::new(
        "stmt".to_string(),
        test_prepared_stmt("SELECT $1"),
        vec![Type::INT8],
    ));
    let mut portal: Portal<PreparedStatement> = Portal::default();
    portal.name = "portal".to_string();
    portal.statement = stmt;
    portal.parameter_format = Format::UnifiedBinary;
    portal.parameters = vec![Some(Bytes::copy_from_slice(&1i64.to_be_bytes()))];
    portal.result_column_format = Format::UnifiedText;

    let values = decode_parameters(&portal).unwrap();
    assert_eq!(values, vec![Some(Value::Int64(1))]);
}

#[test]
fn test_decode_parameters_escapes_single_quotes_in_text() {
    let stmt = Arc::new(StoredStatement::new(
        "stmt".to_string(),
        test_prepared_stmt("SELECT $1"),
        vec![Type::TEXT],
    ));
    let mut portal: Portal<PreparedStatement> = Portal::default();
    portal.name = "portal".to_string();
    portal.statement = stmt;
    portal.parameter_format = Format::UnifiedText;
    portal.parameters = vec![Some(Bytes::from_static(b"O'Reilly"))];
    portal.result_column_format = Format::UnifiedText;

    let values = decode_parameters(&portal).unwrap();
    assert_eq!(values, vec![Some(Value::Text("O'Reilly".to_string()))]);
}

#[test]
fn test_decode_parameters_int4_text_format_renders_int32() {
    let stmt = Arc::new(StoredStatement::new(
        "stmt".to_string(),
        test_prepared_stmt("SELECT $1"),
        vec![Type::INT4],
    ));
    let mut portal: Portal<PreparedStatement> = Portal::default();
    portal.name = "portal".to_string();
    portal.statement = stmt;
    portal.parameter_format = Format::UnifiedText;
    portal.parameters = vec![Some(Bytes::from_static(b"42"))];
    portal.result_column_format = Format::UnifiedText;

    let values = decode_parameters(&portal).unwrap();
    assert_eq!(values, vec![Some(Value::Int32(42))]);
}

#[test]
fn test_decode_parameters_int4_text_format_invalid_errors() {
    let stmt = Arc::new(StoredStatement::new(
        "stmt".to_string(),
        test_prepared_stmt("SELECT $1"),
        vec![Type::INT4],
    ));
    let mut portal: Portal<PreparedStatement> = Portal::default();
    portal.name = "portal".to_string();
    portal.statement = stmt;
    portal.parameter_format = Format::UnifiedText;
    portal.parameters = vec![Some(Bytes::from_static(b"not-a-number"))];
    portal.result_column_format = Format::UnifiedText;

    let err = decode_parameters(&portal).unwrap_err();
    match err {
        PgWireError::UserError(info) => {
            assert_eq!(info.code, "22P02");
            assert!(info.message.contains("invalid input syntax"));
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn describe_execute_metadata_agreement_for_select() {
    assert_describe_execute_metadata_agreement(
        "SELECT id, name FROM t WHERE id = $1",
        vec![
            ("id".to_string(), DataType::Int32, None),
            ("name".to_string(), DataType::Text, None),
        ],
        vec![Type::INT4],
    )
    .await;
}

#[tokio::test]
async fn describe_execute_metadata_agreement_for_insert_returning() {
    assert_describe_execute_metadata_agreement(
        "INSERT INTO t VALUES ($1, $2) RETURNING id",
        vec![("id".to_string(), DataType::Int32, None)],
        vec![Type::INT4, Type::TEXT],
    )
    .await;
}

#[tokio::test]
async fn describe_execute_metadata_agreement_for_update_returning() {
    assert_describe_execute_metadata_agreement(
        "UPDATE t SET name = $1 RETURNING name",
        vec![("name".to_string(), DataType::Text, None)],
        vec![Type::TEXT],
    )
    .await;
}

#[test]
fn test_decode_parameters_uuid_binary_format() {
    let stmt = Arc::new(StoredStatement::new(
        "stmt".to_string(),
        test_prepared_stmt("SELECT $1"),
        vec![Type::UUID],
    ));
    let uuid = uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").expect("valid uuid");
    let mut portal: Portal<PreparedStatement> = Portal::default();
    portal.name = "portal".to_string();
    portal.statement = stmt;
    portal.parameter_format = Format::UnifiedBinary;
    portal.parameters = vec![Some(Bytes::copy_from_slice(uuid.as_bytes()))];
    portal.result_column_format = Format::UnifiedText;

    let values = decode_parameters(&portal).unwrap();
    assert_eq!(values, vec![Some(Value::Uuid(*uuid.as_bytes()))]);
}

#[test]
fn test_decode_parameters_date_binary_format() {
    let stmt = Arc::new(StoredStatement::new(
        "stmt".to_string(),
        test_prepared_stmt("SELECT $1"),
        vec![Type::DATE],
    ));
    let mut portal: Portal<PreparedStatement> = Portal::default();
    portal.name = "portal".to_string();
    portal.statement = stmt;
    portal.parameter_format = Format::UnifiedBinary;
    // Postgres DATE binary: i32 days since 2000-01-01.
    portal.parameters = vec![Some(Bytes::copy_from_slice(&1i32.to_be_bytes()))];
    portal.result_column_format = Format::UnifiedText;

    let values = decode_parameters(&portal).unwrap();
    // 1 day after 2000-01-01 = day 10958 since Unix epoch (10957 + 1)
    assert_eq!(values, vec![Some(Value::Date(10958))]);
}

#[test]
fn test_decode_parameters_unsupported_binary_type_returns_feature_not_supported() {
    // Use a type that has no binary parameter decoding support (e.g., POINT)
    let stmt = Arc::new(StoredStatement::new(
        "stmt".to_string(),
        test_prepared_stmt("SELECT $1"),
        vec![Type::POINT],
    ));
    let mut portal: Portal<PreparedStatement> = Portal::default();
    portal.name = "portal".to_string();
    portal.statement = stmt;
    portal.parameter_format = Format::UnifiedBinary;
    portal.parameters = vec![Some(Bytes::from_static(b"\x00\x00"))];
    portal.result_column_format = Format::UnifiedText;

    let err = decode_parameters(&portal).unwrap_err();
    match err {
        PgWireError::UserError(info) => {
            assert_eq!(info.code, "0A000");
            assert!(info.message.contains("unsupported binary parameter type"));
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn test_decode_parameters_jsonb_binary() {
    // JSONB binary: version byte (0x01) + JSON text
    let stmt = Arc::new(StoredStatement::new(
        "stmt".to_string(),
        test_prepared_stmt("SELECT $1"),
        vec![Type::JSONB],
    ));
    let mut portal: Portal<PreparedStatement> = Portal::default();
    portal.name = "portal".to_string();
    portal.statement = stmt;
    portal.parameter_format = Format::UnifiedBinary;
    portal.parameters = vec![Some(Bytes::from_static(b"\x01{\"a\":1}"))];
    portal.result_column_format = Format::UnifiedText;

    let values = decode_parameters(&portal).unwrap();
    assert_eq!(values, vec![Some(Value::Jsonb("{\"a\":1}".to_string()))]);
}

#[test]
fn test_decode_parameters_jsonb_binary_invalid_version_errors() {
    let stmt = Arc::new(StoredStatement::new(
        "stmt".to_string(),
        test_prepared_stmt("SELECT $1"),
        vec![Type::JSONB],
    ));
    let mut portal: Portal<PreparedStatement> = Portal::default();
    portal.name = "portal".to_string();
    portal.statement = stmt;
    portal.parameter_format = Format::UnifiedBinary;
    portal.parameters = vec![Some(Bytes::from_static(b"\x02{\"a\":1}"))];
    portal.result_column_format = Format::UnifiedText;

    let err = decode_parameters(&portal).unwrap_err();
    match err {
        PgWireError::UserError(info) => {
            assert_eq!(info.code, "22P02");
            assert!(info
                .message
                .contains("unsupported JSONB wire format version"));
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn test_decode_parameters_jsonb_binary_invalid_json_errors() {
    let stmt = Arc::new(StoredStatement::new(
        "stmt".to_string(),
        test_prepared_stmt("SELECT $1"),
        vec![Type::JSONB],
    ));
    let mut portal: Portal<PreparedStatement> = Portal::default();
    portal.name = "portal".to_string();
    portal.statement = stmt;
    portal.parameter_format = Format::UnifiedBinary;
    // Version byte 0x01 + invalid JSON content
    portal.parameters = vec![Some(Bytes::from_static(b"\x01not json at all"))];
    portal.result_column_format = Format::UnifiedText;

    let err = decode_parameters(&portal).unwrap_err();
    match err {
        PgWireError::UserError(info) => {
            assert_eq!(info.code, "22P02");
            assert!(info.message.contains("invalid input syntax"));
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn test_decode_parameters_jsonb_binary_canonicalizes() {
    let stmt = Arc::new(StoredStatement::new(
        "stmt".to_string(),
        test_prepared_stmt("SELECT $1"),
        vec![Type::JSONB],
    ));
    let mut portal: Portal<PreparedStatement> = Portal::default();
    portal.name = "portal".to_string();
    portal.statement = stmt;
    portal.parameter_format = Format::UnifiedBinary;
    // Version byte 0x01 + non-canonical JSON (extra whitespace)
    portal.parameters = vec![Some(Bytes::from_static(b"\x01{ \"b\" : 1 , \"a\" : 2 }"))];
    portal.result_column_format = Format::UnifiedText;

    let values = decode_parameters(&portal).unwrap();
    // serde_json roundtrip normalizes whitespace; key order depends on serde_json internals
    match &values[0] {
        Some(Value::Jsonb(s)) => {
            let reparsed: serde_json::Value = serde_json::from_str(s).unwrap();
            assert_eq!(reparsed["a"], serde_json::json!(2));
            assert_eq!(reparsed["b"], serde_json::json!(1));
            // Verify whitespace is stripped (no spaces around colons/commas)
            assert!(!s.contains(" : "));
            assert!(!s.contains(" , "));
        }
        other => panic!("expected Jsonb value, got {other:?}"),
    }
}

#[test]
fn test_decode_parameters_jsonb_binary_empty_after_version_byte() {
    let stmt = Arc::new(StoredStatement::new(
        "stmt".to_string(),
        test_prepared_stmt("SELECT $1"),
        vec![Type::JSONB],
    ));
    let mut portal: Portal<PreparedStatement> = Portal::default();
    portal.name = "portal".to_string();
    portal.statement = stmt;
    portal.parameter_format = Format::UnifiedBinary;
    // Version byte 0x01 + empty JSON body
    portal.parameters = vec![Some(Bytes::from_static(b"\x01"))];
    portal.result_column_format = Format::UnifiedText;

    let err = decode_parameters(&portal).unwrap_err();
    match err {
        PgWireError::UserError(info) => {
            assert_eq!(info.code, "22P02");
            assert!(info.message.contains("invalid input syntax"));
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

// ── Encode value tests ──────────────────────────────────────────────────────

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
fn test_encode_value_date_binary_uses_unix_epoch_days() {
    let fields = Arc::new(vec![FieldInfo::new(
        "d".to_string(),
        None,
        None,
        Type::DATE,
        FieldFormat::Binary,
    )]);
    let mut encoder = DataRowEncoder::new(fields);
    let tz = crate::types::timestamp::TimeZoneSpec::parse("UTC");
    encode_value(
        &mut encoder,
        &Value::Date(0),
        Some(&DataType::Date),
        tz,
        FieldFormat::Binary,
    )
    .unwrap();
    let row = encoder.finish().unwrap();

    let mut data = row.data;
    assert_eq!(data.get_i32(), 4);
    // PostgreSQL DATE binary is days since 2000-01-01, so 1970-01-01 = -10957.
    assert_eq!(data.get_i32(), -10_957);
}

#[test]
fn test_encode_value_interval_binary_preserves_days_and_remainder() {
    let fields = Arc::new(vec![FieldInfo::new(
        "iv".to_string(),
        None,
        None,
        Type::INTERVAL,
        FieldFormat::Binary,
    )]);
    let mut encoder = DataRowEncoder::new(fields);
    let tz = crate::types::timestamp::TimeZoneSpec::parse("UTC");
    let iv = crate::types::IntervalValue::new(2, 2 * 86_400_000 + 3 * 3_600_000 + 123);
    encode_value(
        &mut encoder,
        &Value::Interval(iv),
        Some(&DataType::Interval),
        tz,
        FieldFormat::Binary,
    )
    .unwrap();
    let row = encoder.finish().unwrap();

    let mut data = row.data;
    assert_eq!(data.get_i32(), 16);
    assert_eq!(data.get_i64(), (3 * 3_600_000 + 123) * 1000);
    assert_eq!(data.get_i32(), 2);
    assert_eq!(data.get_i32(), 2);
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

// ── fs9 infer tests (import directly from crate::sql::table_functions) ──────

#[tokio::test]
async fn infer_fs9_schema_directory_mode() {
    let dir = unique_fs9_infer_base("dir");
    fs::write(dir.join("a.txt"), b"x").expect("write");

    let path = format!("{}/", dir.display());
    let sql = format!("SELECT * FROM extensions.fs9('{path}')");
    let args = extract_fs9_args(&sql);

    let schema = crate::sql::table_functions::infer_fs9_table_function_schema(&args, true)
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

    let schema = crate::sql::table_functions::infer_fs9_table_function_schema(&args, true)
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

    let schema = crate::sql::table_functions::infer_fs9_table_function_schema(&args, true)
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

    let schema = crate::sql::table_functions::infer_fs9_table_function_schema(&args, true)
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

    let schema = crate::sql::table_functions::infer_fs9_table_function_schema(&args, true)
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

    let schema = crate::sql::table_functions::infer_fs9_table_function_schema(&args, false)
        .await
        .expect("schema");
    let cols: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();
    assert_eq!(cols, vec!["_line_number", "line", "_path"]);
    assert_eq!(schema.columns[1].data_type, DataType::Text);

    cleanup_dir(&dir);
}

// ── Deep nesting guard for count_sql_parameters ─────────────────────────────

#[test]
fn test_deep_nested_subquery_parameter_scanning() {
    // Regression guard: a deeply-nested subquery must not overflow the stack
    // in count_sql_parameters. It is an iterative scanner, but this test
    // locks in the guarantee.
    let depth = 50;
    let mut sql = String::from("SELECT $1::int AS v");
    for i in 1..=depth {
        sql = format!("SELECT * FROM ({sql}) t{i}");
    }

    assert_eq!(count_sql_parameters(&sql), 1);
}

// ── Regression tests: on_parse invariants ───────────────────────────────────

#[test]
fn test_is_data_statement_identifies_select() {
    use super::dynamic::is_data_statement;
    assert!(is_data_statement("SELECT 1"));
    assert!(is_data_statement("  SELECT\n1"));
    assert!(is_data_statement(
        "WITH cte AS (SELECT 1) SELECT * FROM cte"
    ));
    assert!(is_data_statement("(SELECT 1)"));
}

#[test]
fn test_is_data_statement_identifies_dml() {
    use super::dynamic::is_data_statement;
    assert!(is_data_statement("INSERT INTO t VALUES (1)"));
    assert!(is_data_statement("UPDATE t SET x = 1"));
    assert!(is_data_statement("DELETE FROM t WHERE id = 1"));
}

#[test]
fn test_is_data_statement_rejects_utility() {
    use super::dynamic::is_data_statement;
    assert!(!is_data_statement("CREATE TABLE t (id INT)"));
    assert!(!is_data_statement("SET search_path TO public"));
    assert!(!is_data_statement("SHOW server_version"));
    assert!(!is_data_statement("DROP TABLE IF EXISTS t"));
    assert!(!is_data_statement("EXPLAIN SELECT 1"));
}

#[test]
fn test_is_data_statement_returns_false_for_unparseable() {
    use super::dynamic::is_data_statement;
    assert!(!is_data_statement("SELCT 1"));
    assert!(!is_data_statement(""));
}

#[test]
fn test_reject_unanalyzed_data_statement_returns_xx000() {
    use super::dynamic::reject_unanalyzed_if_needed;
    let err = reject_unanalyzed_if_needed("SELECT 1", 0, "test reason");
    assert!(err.is_some());
    match err.unwrap() {
        PgWireError::UserError(info) => {
            assert_eq!(info.code, "XX000");
            assert!(info.message.contains("cannot describe data statement"));
            assert!(info.message.contains("test reason"));
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn test_reject_unanalyzed_data_statement_with_params_still_xx000() {
    // Per feedback #1: data statement check takes priority over param check
    use super::dynamic::reject_unanalyzed_if_needed;
    let err = reject_unanalyzed_if_needed("SELECT $1", 1, "infra failure");
    assert!(err.is_some());
    match err.unwrap() {
        PgWireError::UserError(info) => {
            assert_eq!(info.code, "XX000");
            assert!(info.message.contains("cannot describe data statement"));
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn test_reject_unanalyzed_utility_with_params_returns_42p02() {
    use super::dynamic::reject_unanalyzed_if_needed;
    let err = reject_unanalyzed_if_needed("SET search_path TO $1", 1, "test reason");
    assert!(err.is_some());
    match err.unwrap() {
        PgWireError::UserError(info) => {
            assert_eq!(info.code, "42P02");
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn test_reject_unanalyzed_utility_no_params_returns_none() {
    use super::dynamic::reject_unanalyzed_if_needed;
    assert!(reject_unanalyzed_if_needed("SET search_path TO public", 0, "test").is_none());
    assert!(reject_unanalyzed_if_needed("SHOW server_version", 0, "test").is_none());
    assert!(reject_unanalyzed_if_needed("CREATE TABLE t (id INT)", 0, "test").is_none());
}

// ── Regression tests: static utility Describe ───────────────────────────────

#[test]
fn test_utility_describe_show_variable() {
    use super::dynamic::utility_describe_fields;
    let fields = utility_describe_fields("SHOW server_version");
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].name(), "server_version");
    assert_eq!(fields[0].datatype(), &Type::TEXT);
}

#[test]
fn test_utility_describe_show_search_path() {
    use super::dynamic::utility_describe_fields;
    let fields = utility_describe_fields("SHOW search_path");
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].name(), "search_path");
    assert_eq!(fields[0].datatype(), &Type::TEXT);
}

#[test]
fn test_utility_describe_show_tables() {
    use super::dynamic::utility_describe_fields;
    let fields = utility_describe_fields("SHOW TABLES");
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].name(), "table_name");
    assert_eq!(fields[0].datatype(), &Type::TEXT);
}

#[test]
fn test_utility_describe_explain() {
    use super::dynamic::utility_describe_fields;
    let fields = utility_describe_fields("EXPLAIN SELECT 1");
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].name(), "QUERY PLAN");
    assert_eq!(fields[0].datatype(), &Type::TEXT);
}

#[test]
fn test_utility_describe_ddl_returns_empty() {
    use super::dynamic::utility_describe_fields;
    assert!(utility_describe_fields("CREATE TABLE t (id INT)").is_empty());
    assert!(utility_describe_fields("SET search_path TO public").is_empty());
    assert!(utility_describe_fields("DROP TABLE IF EXISTS t").is_empty());
}

#[test]
fn test_utility_describe_unparseable_returns_empty() {
    use super::dynamic::utility_describe_fields;
    assert!(utility_describe_fields("SELCT 1").is_empty());
    assert!(utility_describe_fields("").is_empty());
}

#[test]
fn test_utility_describe_show_all() {
    use super::dynamic::utility_describe_fields;
    let fields = utility_describe_fields("SHOW ALL");
    assert_eq!(fields.len(), 3);
    assert_eq!(fields[0].name(), "name");
    assert_eq!(fields[0].datatype(), &Type::TEXT);
    assert_eq!(fields[1].name(), "setting");
    assert_eq!(fields[1].datatype(), &Type::TEXT);
    assert_eq!(fields[2].name(), "description");
    assert_eq!(fields[2].datatype(), &Type::TEXT);
}

#[test]
fn test_utility_describe_show_all_case_insensitive() {
    use super::dynamic::utility_describe_fields;
    let fields = utility_describe_fields("SHOW all");
    assert_eq!(fields.len(), 3);
    assert_eq!(fields[0].name(), "name");
    assert_eq!(fields[1].name(), "setting");
    assert_eq!(fields[2].name(), "description");
}

// ── AuthenticatedState invariant tests (#1021) ───────────────────────────────

/// Verify that `auth()` panics on a handler that was never authenticated.
/// This is the structural invariant: the pgwire state machine guarantees
/// query methods are only called after authentication, and `auth()` asserts it.
#[tokio::test]
#[should_panic(expected = "BUG: query method called before authentication completed")]
async fn auth_panics_before_authentication() {
    let handler = test_dynamic_handler();
    let _ = handler.auth();
}

// ── JSONB canonicalization regression tests ──────────────────────────────────

#[test]
fn test_encode_jsonb_text_canonical() {
    // Compact stored format should produce canonical output with spaces
    assert_eq!(
        encode_value_to_string(
            &Value::Jsonb(r#"{"b":1,"a":2}"#.to_string()),
            Some(&DataType::Jsonb)
        ),
        r#"{"a": 2, "b": 1}"#
    );
}

#[test]
fn test_encode_jsonb_binary_canonical() {
    use crate::types::DataType;
    use pgwire::api::results::FieldInfo;
    let fields = Arc::new(vec![FieldInfo::new(
        "j".to_string(),
        None,
        None,
        Type::JSONB,
        FieldFormat::Binary,
    )]);
    let mut encoder = DataRowEncoder::new(fields);
    let tz = crate::types::timestamp::TimeZoneSpec::parse("UTC");
    encode_value(
        &mut encoder,
        &Value::Jsonb(r#"{"b":1,"a":2}"#.to_string()),
        Some(&DataType::Jsonb),
        tz,
        FieldFormat::Binary,
    )
    .unwrap();
    let row = encoder.finish().unwrap();
    let mut data = row.data;
    let len = data.get_i32();
    assert!(len > 0);
    let bytes = data.copy_to_bytes(len as usize);
    // First byte is JSONB version byte (0x01), rest is canonical text
    assert_eq!(bytes[0], 0x01);
    let json_text = std::str::from_utf8(&bytes[1..]).unwrap();
    assert_eq!(json_text, r#"{"a": 2, "b": 1}"#);
}
