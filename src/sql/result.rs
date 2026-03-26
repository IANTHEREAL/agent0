//! SQL execution result types

use crate::model::{DataType, Row};
use futures::stream::BoxStream;
use std::sync::Arc;

/// Opaque wrapper around a boxed async row stream for streaming CTAS.
pub struct RowStream(pub BoxStream<'static, anyhow::Result<Row>>);

impl std::fmt::Debug for RowStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<row_stream>")
    }
}

/// Result of executing a SQL statement
#[derive(Debug)]
pub enum ExecuteResult {
    /// SELECT result with rows
    Select {
        columns: Vec<String>,
        column_types: Option<Vec<DataType>>,
        rows: Vec<Row>,
        timezone: Arc<str>,
    },
    /// CREATE TABLE result
    CreateTable,
    /// DROP TABLE result
    DropTable,
    /// TRUNCATE TABLE result
    TruncateTable,
    /// ALTER TABLE result
    AlterTable,
    /// ALTER SEQUENCE result
    AlterSequence,
    /// ALTER FUNCTION result
    AlterFunction,
    /// ALTER INDEX result
    AlterIndex,
    /// CREATE INDEX result
    CreateIndex,
    DropIndex,
    CreateView,
    DropView,
    CreateMaterializedView,
    DropMaterializedView,
    RefreshMaterializedView,
    CreateProcedure,
    DropProcedure,
    CreateFunction,
    DropFunction,
    CreateTrigger,
    DropTrigger,
    CreateExtension,
    DropExtension,
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
        severity: String,
        sqlstate: String,
    },
    /// Streaming SELECT for CTAS — consumed by DDL, never sent over wire.
    SelectStream {
        columns: Vec<String>,
        column_types: Vec<DataType>,
        stream: RowStream,
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
