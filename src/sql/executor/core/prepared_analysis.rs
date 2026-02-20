//! Prepared statement analysis: freeze semantics at Parse time.
//!
//! `Executor::analyze_for_prepared()` runs the same pipeline as execution
//! (view expansion → catalog snapshot → Analyzer → rewriter) but returns
//! the frozen IR instead of executing it.  The protocol handler wraps
//! the result into `PreparedStatement` / `PreparedExec`.

use super::catalog_prefetch::{build_catalog_snapshot, build_catalog_snapshot_for_statement};
use super::view_rewrite::expand_views_in_query;
use super::*;
use crate::sql::analyzer::types::{AnalyzedQuery, AnalyzedStatement};
use crate::sql::analyzer::Analyzer;
use crate::sql::error::SqlError;

/// Result of analyzing a SQL statement for prepared execution.
pub enum PreparedAnalysis {
    /// SELECT / set operation / VALUES — analyzed query IR.
    Query {
        analyzed: AnalyzedQuery,
        locks: Vec<sqlparser::ast::LockClause>,
        select_into: Option<sqlparser::ast::SelectInto>,
        output_schema: Vec<(String, DataType)>,
        param_types: Vec<DataType>,
        base_table_names: Vec<String>,
        table_versions: Vec<(String, u64)>,
    },
    /// INSERT / UPDATE / DELETE — analyzed DML IR.
    Dml {
        analyzed: AnalyzedStatement,
        output_schema: Vec<(String, DataType)>,
        param_types: Vec<DataType>,
        table_versions: Vec<(String, u64)>,
    },
    /// DDL / utility / non-analyzable statement.
    Utility,
}

impl Executor {
    /// Analyze a SQL statement at Parse time to produce frozen execution IR.
    ///
    /// Uses the same pipeline shape as runtime execution:
    /// - SELECT: view expansion → catalog snapshot → Analyzer → rewriter
    /// - DML: catalog snapshot → Analyzer
    ///
    /// Returns `PreparedAnalysis` with the analyzed IR and finalized parameter types.
    /// The protocol handler constructs `PreparedStatement` from this.
    pub async fn analyze_for_prepared(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        search_path: &[String],
        sql: &str,
        param_count: usize,
        client_oids: &[Option<DataType>],
    ) -> Result<PreparedAnalysis> {
        let statements = parse_sql(sql)?;
        if statements.is_empty() {
            return Ok(PreparedAnalysis::Utility);
        }
        let stmt = &statements[0];

        match stmt {
            Statement::Query(query) => {
                // Pipeline matches analyze_rewrite.rs + select/analyzed/mod.rs:
                // 1. View expansion
                let expanded =
                    expand_views_in_query(self.store().as_ref(), txn, db_id, search_path, query)
                        .await?;

                // 2. Catalog snapshot (empty CTEs at Parse time)
                let catalog = build_catalog_snapshot(
                    self.store().as_ref(),
                    txn,
                    db_id,
                    search_path,
                    self.tenant_keyspace(),
                    &expanded,
                    &HashMap::new(),
                )
                .await?;

                // 3. Analyzer with parameter context
                let mut analyzer = Analyzer::new_with_params(&catalog, param_count, client_oids);
                let analyzed = stacker::maybe_grow(128 * 1024 * 1024, 256 * 1024 * 1024, || {
                    analyzer.analyze_query(&expanded)
                })
                .map_err(SqlError::from)?;

                // 4. Rewriter
                let rewritten = stacker::maybe_grow(128 * 1024 * 1024, 256 * 1024 * 1024, || {
                    crate::sql::rewriter::rewrite_query(analyzed)
                });

                // 5. Finalize parameter types
                let param_types = analyzer.finalize_param_types().map_err(SqlError::from)?;
                let output_schema = rewritten.output_schema.clone();

                // 6. Collect base table names for RBAC
                let base_table_names = catalog
                    .base_table_full_names()
                    .into_iter()
                    .map(|s| s.to_string())
                    .collect();
                let table_versions = catalog.base_table_versions();

                // 7. Extract locks + SELECT INTO from original AST
                let locks = query.locks.clone();
                let select_into = match &*query.body {
                    SetExpr::Select(s) => s.into.clone(),
                    _ => None,
                };

                crate::sql::stack_safety::drop_on_grown_stack(expanded);

                Ok(PreparedAnalysis::Query {
                    analyzed: rewritten,
                    locks,
                    select_into,
                    output_schema,
                    param_types,
                    base_table_names,
                    table_versions,
                })
            }

            Statement::Insert { .. } | Statement::Update { .. } | Statement::Delete { .. } => {
                // Pipeline matches stmt_dml.rs
                let catalog = build_catalog_snapshot_for_statement(
                    self.store().as_ref(),
                    txn,
                    db_id,
                    search_path,
                    self.tenant_keyspace(),
                    stmt,
                )
                .await?;

                let mut analyzer = Analyzer::new_with_params(&catalog, param_count, client_oids);
                let analyzed_stmt = analyzer.analyze_statement(stmt).map_err(SqlError::from)?;
                let param_types = analyzer.finalize_param_types().map_err(SqlError::from)?;

                let output_schema = match &analyzed_stmt {
                    AnalyzedStatement::Query(q) => q.output_schema.clone(),
                    AnalyzedStatement::Insert(i) => returning_schema(&i.returning),
                    AnalyzedStatement::Update(u) => returning_schema(&u.returning),
                    AnalyzedStatement::Delete(d) => returning_schema(&d.returning),
                };
                let table_versions = catalog.base_table_versions();

                Ok(PreparedAnalysis::Dml {
                    analyzed: analyzed_stmt,
                    output_schema,
                    param_types,
                    table_versions,
                })
            }

            _ => {
                if param_count > 0 {
                    Err(SqlError::InvalidParameterUsage {
                        index: 1,
                        context: "utility statements do not support parameters".to_string(),
                    }
                    .into())
                } else {
                    Ok(PreparedAnalysis::Utility)
                }
            }
        }
    }
}

fn returning_schema(
    ret: &Option<Vec<crate::sql::analyzer::types::AnalyzedProjection>>,
) -> Vec<(String, DataType)> {
    match ret {
        Some(projections) => projections
            .iter()
            .map(|p| (p.output_name.clone(), p.expr.data_type.clone()))
            .collect(),
        None => vec![],
    }
}
