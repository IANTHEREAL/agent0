//! SQL execution result types

use crate::types::{DataType, Row, TableSchema};
use std::sync::Arc;

/// Result of executing a SQL statement
#[derive(Debug)]
#[allow(dead_code)] // variant name fields are structural, for future logging/error reporting
pub enum ExecuteResult {
    /// SELECT result with rows
    Select {
        columns: Vec<String>,
        column_types: Option<Vec<DataType>>,
        rows: Vec<Row>,
        timezone: Arc<str>,
    },
    /// CREATE TABLE result
    CreateTable {
        table_name: String,
    },
    /// DROP TABLE result  
    DropTable {
        table_name: String,
    },
    /// TRUNCATE TABLE result
    TruncateTable {
        table_name: String,
    },
    /// ALTER TABLE result
    AlterTable {
        table_name: String,
    },
    /// ALTER SEQUENCE result
    AlterSequence {
        sequence_name: String,
    },
    /// ALTER FUNCTION result
    AlterFunction {
        function_name: String,
    },
    /// ALTER INDEX result
    AlterIndex {
        index_name: String,
    },
    /// CREATE INDEX result
    CreateIndex {
        index_name: String,
    },
    DropIndex {
        index_name: String,
    },
    CreateView {
        view_name: String,
    },
    DropView {
        view_name: String,
    },
    CreateMaterializedView {
        view_name: String,
    },
    DropMaterializedView {
        view_name: String,
    },
    RefreshMaterializedView {
        view_name: String,
    },
    CreateProcedure {
        proc_name: String,
    },
    DropProcedure {
        proc_name: String,
    },
    CreateFunction {
        func_name: String,
    },
    DropFunction {
        func_name: String,
    },
    CreateTrigger {
        trigger_name: String,
        table_name: String,
    },
    DropTrigger {
        trigger_name: String,
        table_name: String,
    },
    CreateExtension {
        ext_name: String,
    },
    DropExtension {
        ext_name: String,
    },
    Call,
    CreateRole,
    AlterRole,
    DropRole,
    Grant,
    Revoke,

    /// INSERT result with affected row count
    Insert {
        affected_rows: u64,
    },
    /// DELETE result with affected row count
    Delete {
        affected_rows: u64,
    },
    /// UPDATE result with affected row count
    Update {
        affected_rows: u64,
    },
    /// SHOW TABLES result
    ShowTables {
        tables: Vec<String>,
    },
    /// DESCRIBE table result
    #[allow(dead_code)]
    // variant constructed in protocol layer, field for schema introspection
    Describe {
        schema: TableSchema,
    },
    /// Command completed successfully (no rowset).
    CommandComplete {
        tag: &'static str,
    },
    /// Transaction block started successfully (BEGIN / START TRANSACTION).
    ///
    /// This maps to a pgwire `TransactionStart` response so the client can
    /// track its transaction status correctly.
    TransactionStart {
        tag: &'static str,
    },
    /// Transaction block ended successfully (COMMIT / ROLLBACK).
    ///
    /// This maps to a pgwire `TransactionEnd` response so the client can track
    /// its transaction status correctly.
    TransactionEnd {
        tag: &'static str,
    },
    /// Empty result — reserved for truly empty queries (empty string / whitespace / only `;`).
    /// Do NOT use for unsupported or no-op statements; use `CommandComplete` or return an error.
    Empty,
    /// Server notice message (sent as NoticeResponse on the wire)
    Notice {
        message: String,
    },
}

/// Results from executing multiple statements in a batch
#[derive(Debug)]
pub struct ExecuteResults(pub Vec<ExecuteResult>);

impl ExecuteResults {
    pub fn single(result: ExecuteResult) -> Self {
        ExecuteResults(vec![result])
    }

    pub fn into_vec(self) -> Vec<ExecuteResult> {
        self.0
    }

    pub fn last(self) -> ExecuteResult {
        self.0.into_iter().last().unwrap_or(ExecuteResult::Empty)
    }
}
