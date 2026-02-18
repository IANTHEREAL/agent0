//! DML statement sub-dispatcher

use super::catalog_prefetch::build_catalog_snapshot_for_statement;
use super::*;
use crate::auth::Privilege;
use crate::sql::analyzer::types::AnalyzedStatement;
use crate::sql::analyzer::Analyzer;
use crate::sql::error::SqlError;

impl Executor {
    pub(super) async fn execute_dml_statement(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
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
                let mut analyzer = Analyzer::new(&catalog);
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
                let mut analyzer = Analyzer::new(&catalog);
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
                let mut analyzer = Analyzer::new(&catalog);
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
