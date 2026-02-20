//! Prepared statement types for the extended query protocol.
//!
//! `PreparedStatement` is the immutable IR created at Parse time.
//! Both Describe and Execute read from the same instance — single semantic source.

use crate::auth::Privilege;
use crate::sql::analyzer::types::{
    AnalyzedOnConflict, AnalyzedProjection, AnalyzedQuery, AnalyzedStatement,
};
use crate::types::DataType;
use sqlparser::ast::LockClause;

/// Execution plan variant for a prepared statement.
/// Kept in the protocol handler layer; references executor-layer IR types.
#[derive(Debug, Clone, Default)]
pub enum PreparedExec {
    /// SELECT / set operation / VALUES with analyzed query IR.
    AnalyzedQuery {
        analyzed: AnalyzedQuery,
        locks: Vec<LockClause>,
        select_into: Option<sqlparser::ast::SelectInto>,
        required_privileges: Vec<(String, Privilege)>,
    },
    /// INSERT / UPDATE / DELETE with analyzed DML IR.
    AnalyzedDml {
        analyzed: AnalyzedStatement,
        required_privileges: Vec<(String, Privilege)>,
    },
    /// DDL / utility / non-analyzable statement. No params.
    /// Executed via raw SQL at Execute time.
    #[default]
    RawSqlUtility,
}

/// Immutable prepared statement, created at Parse time.
/// Carries execution IR, output schema, and original SQL.
/// Both Describe and Execute read from this — single semantic source (§3.1).
#[derive(Debug, Clone, Default)]
pub struct PreparedStatement {
    /// Original SQL text (diagnostics/display, schema-drift fallback execution).
    pub sql: String,
    /// Execution plan: analyzed IR or raw-SQL fallback.
    pub exec: PreparedExec,
    /// Output column schema: (name, type). Empty for non-row-returning.
    pub output_schema: Vec<(String, DataType)>,
    /// Finalized parameter types from Parse-time analysis.
    /// Threaded to execute-time Analyzer so re-analysis uses the same types.
    pub param_data_types: Vec<DataType>,
    /// Parse-time base-table schema versions used for drift detection.
    /// `(table_full_name, schema_version)`.
    pub table_versions: Vec<(String, u64)>,
}

impl PreparedStatement {
    /// Extract output schema from an AnalyzedStatement.
    pub fn output_schema_from(stmt: &AnalyzedStatement) -> Vec<(String, DataType)> {
        match stmt {
            AnalyzedStatement::Query(q) => q.output_schema.clone(),
            AnalyzedStatement::Insert(i) => returning_schema(&i.returning),
            AnalyzedStatement::Update(u) => returning_schema(&u.returning),
            AnalyzedStatement::Delete(d) => returning_schema(&d.returning),
        }
    }

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

fn returning_schema(ret: &Option<Vec<AnalyzedProjection>>) -> Vec<(String, DataType)> {
    match ret {
        Some(projections) => projections
            .iter()
            .map(|p| (p.output_name.clone(), p.expr.data_type.clone()))
            .collect(),
        None => vec![],
    }
}
