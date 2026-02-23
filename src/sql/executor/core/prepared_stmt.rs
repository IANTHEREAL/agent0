//! Prepared statement immutable runtime contract.
//!
//! These types belong to SQL execution semantics (frozen analyzed IR),
//! even though they are used by the protocol handler.

use crate::auth::Privilege;
use crate::sql::analyzer::types::{AnalyzedOnConflict, AnalyzedQuery, AnalyzedStatement};
use crate::types::DataType;
use sqlparser::ast::LockClause;

/// Execution plan variant for a prepared statement.
#[derive(Debug, Clone, Default)]
pub enum PreparedExec {
    /// SELECT / set operation / VALUES with analyzed query IR.
    AnalyzedQuery {
        analyzed: AnalyzedQuery,
        locks: Vec<LockClause>,
        select_into: Option<sqlparser::ast::SelectInto>,
        required_privileges: Vec<(String, Privilege)>,
        /// Parse-time detection flag for recursive CTEs.
        ///
        /// Recursive CTE runtime over analyzed IR is not implemented yet; Execute
        /// falls back to text path when this is `true`.
        has_recursive_cte: bool,
    },
    /// INSERT / UPDATE / DELETE with analyzed DML IR.
    AnalyzedDml {
        analyzed: AnalyzedStatement,
        required_privileges: Vec<(String, Privilege)>,
    },
    /// DDL / utility / non-analyzable statement. No params.
    #[default]
    RawSqlUtility,
}

/// Immutable prepared statement, created at Parse time.
#[derive(Debug, Clone, Default)]
pub struct PreparedStatement {
    /// Original SQL text (diagnostics/display, schema-drift fallback execution).
    pub sql: String,
    /// Execution plan: analyzed IR or raw-SQL fallback.
    pub exec: PreparedExec,
    /// Output column schema: (name, type). Empty for non-row-returning.
    pub output_schema: Vec<(String, DataType)>,
    /// Finalized parameter types from Parse-time analysis.
    pub param_data_types: Vec<DataType>,
    /// Parse-time base-table schema versions used for drift detection.
    /// `(table_full_name, schema_version)`.
    pub table_versions: Vec<(String, u64)>,
}

impl PreparedStatement {
    /// Compute required privileges from an AnalyzedStatement.
    pub fn compute_privileges(
        stmt: &AnalyzedStatement,
        select_base_tables: &[String],
    ) -> Vec<(String, Privilege)> {
        match stmt {
            AnalyzedStatement::Query(_) => select_base_tables
                .iter()
                .map(|t| (t.to_string(), Privilege::Select))
                .collect(),
            AnalyzedStatement::Insert(ins) => {
                let mut p = vec![(ins.table_name.clone(), Privilege::Insert)];
                if matches!(ins.on_conflict, Some(AnalyzedOnConflict::DoUpdate { .. })) {
                    p.push((ins.table_name.clone(), Privilege::Update));
                }
                p
            }
            AnalyzedStatement::Update(upd) => {
                vec![(upd.table_name.clone(), Privilege::Update)]
            }
            AnalyzedStatement::Delete(del) => {
                vec![(del.table_name.clone(), Privilege::Delete)]
            }
        }
    }
}
