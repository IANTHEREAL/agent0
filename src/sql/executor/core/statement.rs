//! Statement execution

use super::*;
use crate::auth::{Privilege, PrivilegeObject};
use crate::sql::error::SqlError;

impl Executor {
    pub(crate) async fn require_privilege(
        &self,
        txn: &mut Transaction,
        current_role: Option<&str>,
        privilege: Privilege,
        object: PrivilegeObject,
        object_type: &str,
        object_name: String,
    ) -> Result<()> {
        let Some(username) = current_role else {
            // Internal execution path (trigger worker, internal plumbing).
            // Until we have explicit security context propagation for internal
            // statements, bypass RBAC checks when no user is provided.
            return Ok(());
        };

        let ok = self
            .auth_manager
            .check_privilege(txn, username, &privilege, &object)
            .await?;
        if !ok {
            return Err(SqlError::PermissionDenied {
                object_type: object_type.to_string(),
                object_name,
            }
            .into());
        }
        Ok(())
    }

    pub(crate) async fn require_table_privilege(
        &self,
        txn: &mut Transaction,
        current_role: Option<&str>,
        privilege: Privilege,
        table_full_name: &str,
    ) -> Result<()> {
        let (schema, name) = names::parse_full_name(table_full_name)
            .unwrap_or(("public".to_string(), table_full_name.to_string()));
        self.require_privilege(
            txn,
            current_role,
            privilege,
            PrivilegeObject::Table {
                schema: schema.clone(),
                name: name.clone(),
            },
            "table",
            format!("{}.{}", schema, name),
        )
        .await
    }

    /// Execute a parsed SQL statement on a given transaction.
    ///
    /// Return a boxed future so callers (`dispatch`, trigger execution,
    /// procedures, user functions, worker engine) don't embed this large
    /// statement-dispatch future directly into their own async state machines.
    ///
    /// This keeps the stack profile stable as statement arms evolve.
    pub(crate) fn execute_statement_on_txn<'a>(
        &'a self,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut HashMap<String, i64>,
        search_path: &'a [String],
        stmt: &'a Statement,
        current_role: Option<&'a str>,
    ) -> super::BoxStmtFuture<'a> {
        Box::pin(async move {
            self.execute_statement_on_txn_impl(
                txn,
                db_id,
                sequence_values,
                search_path,
                stmt,
                current_role,
            )
            .await
        })
    }

    async fn execute_statement_on_txn_impl(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        stmt: &Statement,
        current_role: Option<&str>,
    ) -> Result<ExecuteResult> {
        match stmt {
            // DDL
            Statement::CreateTable { .. }
            | Statement::CreateIndex { .. }
            | Statement::Drop { .. }
            | Statement::Truncate { .. }
            | Statement::AlterTable { .. }
            | Statement::CreateType { .. }
            | Statement::CreateSchema { .. }
            | Statement::CreateFunction { .. }
            | Statement::CreateProcedure { .. }
            | Statement::CreateSequence { .. }
            | Statement::CreateView { .. }
            | Statement::AlterIndex { .. }
            | Statement::DropFunction { .. } => {
                Box::pin(self.execute_ddl_statement(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    stmt,
                    current_role,
                ))
                .await
            }

            // DML
            Statement::Insert { .. }
            | Statement::Delete { .. }
            | Statement::Update { .. } => {
                Box::pin(self.execute_dml_statement(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    stmt,
                    current_role,
                ))
                .await
            }

            // Query
            Statement::Query(_)
            | Statement::ShowTables { .. }
            | Statement::Explain { .. } => {
                Box::pin(self.execute_query_statement(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    stmt,
                    current_role,
                ))
                .await
            }

            // RBAC
            Statement::CreateRole { .. }
            | Statement::AlterRole { .. }
            | Statement::Grant { .. }
            | Statement::Revoke { .. } => {
                Box::pin(self.execute_rbac_statement(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    stmt,
                    current_role,
                ))
                .await
            }

            // Errors (no .await, zero future cost — stay inline)
            Statement::SetVariable { .. }
            | Statement::SetTimeZone { .. }
            | Statement::SetNames { .. }
            | Statement::SetTransaction { .. } => {
                Err(SqlError::Unsupported("SET is not supported in this context".into()).into())
            }
            Statement::Comment { .. } => {
                Err(SqlError::Unsupported("COMMENT is not supported in this context".into()).into())
            }
            Statement::Copy { .. } => {
                Err(SqlError::Unsupported("COPY is not supported in this context".into()).into())
            }
            _ => Err(SqlError::Unsupported(format!("Unsupported statement: {:?}", stmt)).into()),
        }
    }
}
