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

impl ExecuteResult {
    pub(crate) fn modifies_database(&self) -> bool {
        match self {
            ExecuteResult::CreateTable
            | ExecuteResult::DropTable
            | ExecuteResult::TruncateTable
            | ExecuteResult::AlterTable
            | ExecuteResult::AlterSequence
            | ExecuteResult::AlterFunction
            | ExecuteResult::AlterIndex
            | ExecuteResult::CreateIndex
            | ExecuteResult::DropIndex
            | ExecuteResult::CreateView
            | ExecuteResult::DropView
            | ExecuteResult::CreateMaterializedView
            | ExecuteResult::DropMaterializedView
            | ExecuteResult::RefreshMaterializedView
            | ExecuteResult::CreateProcedure
            | ExecuteResult::DropProcedure
            | ExecuteResult::CreateFunction
            | ExecuteResult::DropFunction
            | ExecuteResult::CreateTrigger
            | ExecuteResult::DropTrigger
            | ExecuteResult::CreateExtension
            | ExecuteResult::DropExtension
            | ExecuteResult::CreateRole
            | ExecuteResult::AlterRole
            | ExecuteResult::DropRole
            | ExecuteResult::Grant
            | ExecuteResult::Revoke => true,
            ExecuteResult::Insert { affected_rows }
            | ExecuteResult::Delete { affected_rows }
            | ExecuteResult::Update { affected_rows } => *affected_rows > 0,
            ExecuteResult::CommandComplete { tag } => command_tag_modifies_database(tag),
            _ => false,
        }
    }
}

fn command_tag_modifies_database(tag: &str) -> bool {
    if tag.eq_ignore_ascii_case("ALTER SYSTEM") {
        return false;
    }

    tag.starts_with("CREATE ")
        || tag.starts_with("DROP ")
        || tag.starts_with("ALTER ")
        || tag.starts_with("TRUNCATE ")
        || tag.starts_with("COMMENT")
        || tag.starts_with("GRANT")
        || tag.starts_with("REVOKE")
        || tag.starts_with("REFRESH MATERIALIZED VIEW")
        || tag.starts_with("ANALYZE")
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

#[cfg(test)]
mod tests {
    use super::ExecuteResult;

    #[test]
    fn modifies_database_classifies_reads_and_writes() {
        assert!(ExecuteResult::Insert { affected_rows: 1 }.modifies_database());
        assert!(ExecuteResult::Update { affected_rows: 1 }.modifies_database());
        assert!(ExecuteResult::Delete { affected_rows: 1 }.modifies_database());
        assert!(!ExecuteResult::Insert { affected_rows: 0 }.modifies_database());
        assert!(!ExecuteResult::Update { affected_rows: 0 }.modifies_database());
        assert!(!ExecuteResult::Delete { affected_rows: 0 }.modifies_database());
        assert!(ExecuteResult::CreateTable.modifies_database());
        assert!(ExecuteResult::CommandComplete { tag: "CREATE TYPE" }.modifies_database());
        assert!(ExecuteResult::CommandComplete { tag: "ANALYZE" }.modifies_database());

        assert!(!ExecuteResult::CommandComplete { tag: "SET" }.modifies_database());
        assert!(!ExecuteResult::CommandComplete {
            tag: "ALTER SYSTEM"
        }
        .modifies_database());
        assert!(!ExecuteResult::Empty.modifies_database());
    }
}
