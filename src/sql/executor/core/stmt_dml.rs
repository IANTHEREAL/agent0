//! DML statement sub-dispatcher

use super::catalog_prefetch::build_catalog_snapshot_for_statement;
use super::*;
use crate::auth::Privilege;
use crate::model::RlsCommand;
use crate::sql::analyzer::types::{AnalyzedOnConflict, AnalyzedStatement};
use crate::sql::analyzer::Analyzer;
use crate::sql::error::SqlError;
use crate::sql::rls::dml::{maybe_build_rls_context, RlsDmlContext};
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
    /// Build an RLS enforcement context for a DML statement, if needed.
    ///
    /// Returns `None` when RLS is disabled, the user is superuser, or the user
    /// is the table owner without FORCE RLS.
    async fn build_rls_dml_context(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_name: &str,
        current_role: Option<&str>,
        command: RlsCommand,
        has_returning: bool,
        has_on_conflict_update: bool,
    ) -> Result<Option<RlsDmlContext>> {
        let role = match current_role {
            Some(r) => r,
            None => return Ok(None),
        };

        // Fetch schema to check rls_enabled / rls_force / owner.
        let schema = match self.store().get_schema(txn, db_id, table_name).await? {
            Some(s) => s,
            None => return Ok(None),
        };

        if !schema.rls_enabled {
            return Ok(None);
        }

        // Check if user is superuser.
        let is_superuser = self
            .auth_manager()
            .get_user(txn, role)
            .await?
            .map(|u| u.is_superuser)
            .unwrap_or(false);

        let qctx = crate::sql::query_context::QueryContext::from_task_locals();

        let store = self.store();
        let table_id = schema.table_id;
        maybe_build_rls_context(
            &schema,
            Some(role),
            is_superuser,
            schema.rls_enabled,
            schema.rls_force,
            command,
            has_returning,
            has_on_conflict_update,
            &qctx,
            || async { store.list_policies_for_table(txn, db_id, table_id).await },
        )
        .await
    }

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
                        let has_on_conflict_update =
                            matches!(ins.on_conflict, Some(AnalyzedOnConflict::DoUpdate { .. }));
                        if has_on_conflict_update {
                            self.require_table_privilege(
                                txn,
                                current_role,
                                Privilege::Update,
                                &ins.table_name,
                            )
                            .await?;
                        }
                        let rls_ctx = self
                            .build_rls_dml_context(
                                txn,
                                db_id,
                                &ins.table_name,
                                current_role,
                                RlsCommand::Insert,
                                ins.returning.is_some(),
                                has_on_conflict_update,
                            )
                            .await?;
                        self.execute_analyzed_insert(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            &ins,
                            rls_ctx.as_ref(),
                        )
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
                        let rls_ctx = self
                            .build_rls_dml_context(
                                txn,
                                db_id,
                                &del.table_name,
                                current_role,
                                RlsCommand::Delete,
                                del.returning.is_some(),
                                false,
                            )
                            .await?;
                        self.execute_analyzed_delete(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            &del,
                            rls_ctx.as_ref(),
                        )
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
                        let rls_ctx = self
                            .build_rls_dml_context(
                                txn,
                                db_id,
                                &upd.table_name,
                                current_role,
                                RlsCommand::Update,
                                upd.returning.is_some(),
                                false,
                            )
                            .await?;
                        self.execute_analyzed_update(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            &upd,
                            rls_ctx.as_ref(),
                        )
                        .await
                    }
                    _ => unreachable!("UPDATE statement should analyze to AnalyzedUpdate"),
                }
            }
            _ => unreachable!("DML dispatcher received non-DML statement"),
        }
    }
}
