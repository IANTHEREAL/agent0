//! SQL execution result types

use crate::types::{DataType, Row, TableSchema};

/// Result of executing a SQL statement
#[derive(Debug)]
#[allow(dead_code)]
pub enum ExecuteResult {
    /// SELECT result with rows
    Select {
        columns: Vec<String>,
        column_types: Option<Vec<DataType>>,
        rows: Vec<Row>,
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
    Describe {
        schema: TableSchema,
    },
    /// Empty result (for unsupported/noop statements)
    Empty,
    /// Skipped statement with warning message
    Skipped {
        message: String,
    },
    /// Server notice message (sent as NoticeResponse on the wire)
    Notice {
        message: String,
    },
}

impl ExecuteResult {
    #[allow(dead_code)]
    pub fn affected_rows(&self) -> u64 {
        match self {
            ExecuteResult::Insert { affected_rows } => *affected_rows,
            ExecuteResult::Delete { affected_rows } => *affected_rows,
            ExecuteResult::Update { affected_rows } => *affected_rows,
            _ => 0,
        }
    }

    #[allow(dead_code)]
    pub fn is_query(&self) -> bool {
        matches!(
            self,
            ExecuteResult::Select { .. }
                | ExecuteResult::ShowTables { .. }
                | ExecuteResult::Describe { .. }
        )
    }
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
