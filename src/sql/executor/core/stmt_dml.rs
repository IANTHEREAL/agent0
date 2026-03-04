//! DML statement sub-dispatcher

use super::catalog_prefetch::build_catalog_snapshot_for_statement;
use super::*;
use crate::auth::Privilege;
use crate::sql::analyzer::types::AnalyzedStatement;
use crate::sql::analyzer::Analyzer;
use crate::sql::error::SqlError;
use crate::sql::sequences::SequenceSession;

/// Create an Analyzer that is aware of extended-query parameters when present.
/// Checks QUERY_PARAMS task-local; if non-empty, creates a param-aware Analyzer
/// so $N placeholders resolve to TypedExprKind::Parameter nodes.
fn make_analyzer<'a>(catalog: &'a crate::sql::analyzer::catalog::CatalogSnapshot) -> Analyzer<'a> {
    let query_params = crate::sql::query_context::QueryContext::current_query_params();
    if !query_params.is_empty() {
        let param_types = crate::sql::query_context::QueryContext::current_query_param_types();
        let client_oids = if param_types.len() == query_params.len() {
            param_types
        } else {
            vec![None; query_params.len()]
        };
        Analyzer::new_with_params(catalog, query_params.len(), &client_oids)
    } else {
        Analyzer::new(catalog)
    }
}

impl Executor {
    pub(super) async fn execute_dml_statement(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut SequenceSession,
        search_path: &[String],
        stmt: &Statement,
        current_role: Option<&str>,
    ) -> Result<ExecuteResult> {
        match stmt {
            Statement::Insert { .. } => {
                // All INSERT variants (VALUES, DEFAULT VALUES, SELECT) use the
                // fully analyzed path.
                let catalog = build_catalog_snapshot_for_statement(
                    self.store().as_ref(),
                    txn,
                    db_id,
                    search_path,
                    self.tenant_keyspace(),
                    stmt,
                )
                .await?;
                let mut analyzer = make_analyzer(&catalog);
                let analyzed = analyzer.analyze_statement(stmt).map_err(SqlError::from)?;
                match analyzed {
                    AnalyzedStatement::Insert(ins) => {
                        self.require_table_privilege(
                            txn,
                            current_role,
                            Privilege::Insert,
                            &ins.table_name,
                        )
                        .await?;
                        if matches!(
                            ins.on_conflict,
                            Some(crate::sql::analyzer::types::AnalyzedOnConflict::DoUpdate { .. })
                        ) {
                            self.require_table_privilege(
                                txn,
                                current_role,
                                Privilege::Update,
                                &ins.table_name,
                            )
                            .await?;
                        }
                        self.execute_analyzed_insert(txn, db_id, sequence_values, search_path, &ins)
                            .await
                    }
                    _ => unreachable!("INSERT statement should analyze to AnalyzedInsert"),
                }
            }
            Statement::Delete { .. } => {
                let catalog = build_catalog_snapshot_for_statement(
                    self.store().as_ref(),
                    txn,
                    db_id,
                    search_path,
                    self.tenant_keyspace(),
                    stmt,
                )
                .await?;
                let mut analyzer = make_analyzer(&catalog);
                let analyzed = analyzer.analyze_statement(stmt).map_err(SqlError::from)?;
                match analyzed {
                    AnalyzedStatement::Delete(del) => {
                        self.require_table_privilege(
                            txn,
                            current_role,
                            Privilege::Delete,
                            &del.table_name,
                        )
                        .await?;
                        self.execute_analyzed_delete(txn, db_id, sequence_values, search_path, &del)
                            .await
                    }
                    _ => unreachable!("DELETE statement should analyze to AnalyzedDelete"),
                }
            }
            Statement::Update { .. } => {
                let catalog = build_catalog_snapshot_for_statement(
                    self.store().as_ref(),
                    txn,
                    db_id,
                    search_path,
                    self.tenant_keyspace(),
                    stmt,
                )
                .await?;
                let mut analyzer = make_analyzer(&catalog);
                let analyzed = analyzer.analyze_statement(stmt).map_err(SqlError::from)?;
                match analyzed {
                    AnalyzedStatement::Update(upd) => {
                        self.require_table_privilege(
                            txn,
                            current_role,
                            Privilege::Update,
                            &upd.table_name,
                        )
                        .await?;
                        self.execute_analyzed_update(txn, db_id, sequence_values, search_path, &upd)
                            .await
                    }
                    _ => unreachable!("UPDATE statement should analyze to AnalyzedUpdate"),
                }
            }
            _ => unreachable!("DML dispatcher received non-DML statement"),
        }
    }
}
