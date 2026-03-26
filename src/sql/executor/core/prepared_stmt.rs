//! Prepared statement immutable runtime contract.
//!
//! These types belong to SQL execution semantics (frozen analyzed IR),
//! even though they are used by the protocol handler.

use crate::auth::Privilege;
use crate::model::DataType;
use crate::sql::analyzer::types::{AnalyzedOnConflict, AnalyzedQuery, AnalyzedStatement};
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
    /// Parse-time base-table schema dependencies for drift detection.
    /// `(table_full_name, table_id, schema_version)`.
    ///
    /// `table_id` enables correct invalidation across DROP+CREATE of the
    /// same table name. `schema_version` detects DDL mutations (including
    /// CREATE/DROP INDEX).
    pub table_versions: Vec<(String, u64, u64)>,
    /// Whether Execute must re-analyze this statement from SQL text under the
    /// current principal instead of reusing frozen prepared IR.
    ///
    /// Phase-1 RLS uses this to force text fallback for statements touching RLS
    /// tables, which avoids caching role-sensitive predicates inside the
    /// immutable prepared representation.
    pub rls_sensitive: bool,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::analyzer::types::{AnalyzedQueryBody, SetOpKind};

    fn empty_analyzed_query() -> AnalyzedQuery {
        AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::SetOperation {
                op: SetOpKind::Union,
                all: false,
                left: Box::new(AnalyzedQuery {
                    ctes: vec![],
                    body: AnalyzedQueryBody::Values(vec![]),
                    order_by: vec![],
                    limit: None,
                    offset: None,
                    output_schema: vec![],
                }),
                right: Box::new(AnalyzedQuery {
                    ctes: vec![],
                    body: AnalyzedQueryBody::Values(vec![]),
                    order_by: vec![],
                    limit: None,
                    offset: None,
                    output_schema: vec![],
                }),
            },
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![],
        }
    }

    #[test]
    fn defaults_to_raw_utility_exec() {
        let exec = PreparedExec::default();
        assert!(matches!(exec, PreparedExec::RawSqlUtility));

        let stmt = PreparedStatement::default();
        assert!(matches!(stmt.exec, PreparedExec::RawSqlUtility));
        assert!(stmt.sql.is_empty());
        assert!(!stmt.rls_sensitive);
    }

    #[test]
    fn compute_privileges_for_query_uses_select_base_tables() {
        let stmt = AnalyzedStatement::Query(empty_analyzed_query());
        let privileges = PreparedStatement::compute_privileges(
            &stmt,
            &["public.t1".to_string(), "app.t2".to_string()],
        );
        assert_eq!(
            privileges,
            vec![
                ("public.t1".to_string(), Privilege::Select),
                ("app.t2".to_string(), Privilege::Select)
            ]
        );
    }

    #[test]
    fn compute_privileges_for_insert_update_delete_variants() {
        use crate::sql::analyzer::types::{
            AnalyzedDelete, AnalyzedInsert, AnalyzedInsertSource, AnalyzedOnConflict,
            AnalyzedUpdate,
        };

        let insert_plain = AnalyzedStatement::Insert(AnalyzedInsert {
            table_name: "public.t".to_string(),
            target_columns: vec![],
            source: AnalyzedInsertSource::DefaultValues,
            on_conflict: None,
            returning: None,
        });
        assert_eq!(
            PreparedStatement::compute_privileges(&insert_plain, &[]),
            vec![("public.t".to_string(), Privilege::Insert)]
        );

        let insert_upsert = AnalyzedStatement::Insert(AnalyzedInsert {
            table_name: "public.t".to_string(),
            target_columns: vec![],
            source: AnalyzedInsertSource::DefaultValues,
            on_conflict: Some(AnalyzedOnConflict::DoUpdate {
                target: None,
                assignments: vec![],
                where_clause: None,
            }),
            returning: None,
        });
        assert_eq!(
            PreparedStatement::compute_privileges(&insert_upsert, &[]),
            vec![
                ("public.t".to_string(), Privilege::Insert),
                ("public.t".to_string(), Privilege::Update)
            ]
        );

        let update_stmt = AnalyzedStatement::Update(AnalyzedUpdate {
            table_name: "app.u".to_string(),
            assignments: vec![],
            from: vec![],
            where_clause: None,
            returning: None,
        });
        assert_eq!(
            PreparedStatement::compute_privileges(&update_stmt, &[]),
            vec![("app.u".to_string(), Privilege::Update)]
        );

        let delete_stmt = AnalyzedStatement::Delete(AnalyzedDelete {
            table_name: "app.d".to_string(),
            using: vec![],
            where_clause: None,
            returning: None,
        });
        assert_eq!(
            PreparedStatement::compute_privileges(&delete_stmt, &[]),
            vec![("app.d".to_string(), Privilege::Delete)]
        );
    }
}
