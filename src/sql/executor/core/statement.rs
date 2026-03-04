//! Statement execution

use super::*;
use crate::auth::{Privilege, PrivilegeObject};
use crate::sql::error::SqlError;
use crate::sql::sequences::SequenceSession;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatementDispatchKind {
    Ddl,
    Dml,
    Query,
    Rbac,
    UnsupportedSet,
    UnsupportedComment,
    UnsupportedCopy,
    UnsupportedOther,
}

fn classify_statement(stmt: &Statement) -> StatementDispatchKind {
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
        | Statement::DropFunction { .. } => StatementDispatchKind::Ddl,

        // DML
        Statement::Insert { .. } | Statement::Delete { .. } | Statement::Update { .. } => {
            StatementDispatchKind::Dml
        }

        // Query
        Statement::Query(_) | Statement::ShowTables { .. } | Statement::Explain { .. } => {
            StatementDispatchKind::Query
        }

        // RBAC
        Statement::CreateRole { .. }
        | Statement::AlterRole { .. }
        | Statement::Grant { .. }
        | Statement::Revoke { .. } => StatementDispatchKind::Rbac,

        // Explicit unsupported statements
        Statement::SetVariable { .. }
        | Statement::SetTimeZone { .. }
        | Statement::SetNames { .. }
        | Statement::SetTransaction { .. } => StatementDispatchKind::UnsupportedSet,
        Statement::Comment { .. } => StatementDispatchKind::UnsupportedComment,
        Statement::Copy { .. } => StatementDispatchKind::UnsupportedCopy,
        _ => StatementDispatchKind::UnsupportedOther,
    }
}

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
        sequence_values: &'a mut SequenceSession,
        search_path: &'a [String],
        stmt: &'a Statement,
        current_role: Option<&'a str>,
    ) -> super::BoxStmtFuture<'a> {
        self.execute_statement_on_txn_with_create_index_with_params(
            txn,
            db_id,
            sequence_values,
            search_path,
            stmt,
            None,
            current_role,
        )
    }

    pub(crate) fn execute_statement_on_txn_with_create_index_with_params<'a>(
        &'a self,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut SequenceSession,
        search_path: &'a [String],
        stmt: &'a Statement,
        create_index_with_params: Option<&'a str>,
        current_role: Option<&'a str>,
    ) -> super::BoxStmtFuture<'a> {
        Box::pin(async move {
            self.execute_statement_on_txn_impl(
                txn,
                db_id,
                sequence_values,
                search_path,
                stmt,
                create_index_with_params,
                current_role,
            )
            .await
        })
    }

    async fn execute_statement_on_txn_impl(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut SequenceSession,
        search_path: &[String],
        stmt: &Statement,
        create_index_with_params: Option<&str>,
        current_role: Option<&str>,
    ) -> Result<ExecuteResult> {
        match classify_statement(stmt) {
            StatementDispatchKind::Ddl => {
                Box::pin(self.execute_ddl_statement(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    stmt,
                    create_index_with_params,
                    current_role,
                ))
                .await
            }

            StatementDispatchKind::Dml => {
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

            StatementDispatchKind::Query => {
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

            StatementDispatchKind::Rbac => {
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

            StatementDispatchKind::UnsupportedSet => {
                Err(SqlError::Unsupported("SET is not supported in this context".into()).into())
            }
            StatementDispatchKind::UnsupportedComment => {
                Err(SqlError::Unsupported("COMMENT is not supported in this context".into()).into())
            }
            StatementDispatchKind::UnsupportedCopy => {
                Err(SqlError::Unsupported("COPY is not supported in this context".into()).into())
            }
            StatementDispatchKind::UnsupportedOther => {
                Err(SqlError::Unsupported(format!("Unsupported statement: {:?}", stmt)).into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{classify_statement, StatementDispatchKind};
    use crate::sql::parse_sql;
    use sqlparser::ast::Statement;

    fn first_stmt(sql: &str) -> Statement {
        let mut stmts = parse_sql(sql).expect("parse sql");
        stmts.remove(0)
    }

    #[test]
    fn classify_statement_routes_major_categories() {
        assert_eq!(
            classify_statement(&first_stmt("CREATE TABLE t(id INT)")),
            StatementDispatchKind::Ddl
        );
        assert_eq!(
            classify_statement(&first_stmt("INSERT INTO t VALUES (1)")),
            StatementDispatchKind::Dml
        );
        assert_eq!(
            classify_statement(&first_stmt("SELECT 1")),
            StatementDispatchKind::Query
        );
        assert_eq!(
            classify_statement(&first_stmt("CREATE ROLE r")),
            StatementDispatchKind::Rbac
        );
    }

    #[test]
    fn classify_statement_routes_explicit_unsupported_cases() {
        assert_eq!(
            classify_statement(&first_stmt("SET application_name = 'x'")),
            StatementDispatchKind::UnsupportedSet
        );
        assert_eq!(
            classify_statement(&first_stmt("COMMENT ON TABLE t IS 'x'")),
            StatementDispatchKind::UnsupportedComment
        );
        assert_eq!(
            classify_statement(&first_stmt("COPY t FROM '/tmp/input.csv'")),
            StatementDispatchKind::UnsupportedCopy
        );
    }

    #[test]
    fn classify_statement_covers_alternate_supported_variants() {
        assert_eq!(
            classify_statement(&first_stmt("CREATE INDEX idx_t_id ON t(id)")),
            StatementDispatchKind::Ddl
        );
        assert_eq!(
            classify_statement(&first_stmt("UPDATE t SET id = 2")),
            StatementDispatchKind::Dml
        );
        assert_eq!(
            classify_statement(&first_stmt("DELETE FROM t")),
            StatementDispatchKind::Dml
        );
        assert_eq!(
            classify_statement(&first_stmt("EXPLAIN SELECT 1")),
            StatementDispatchKind::Query
        );
        assert_eq!(
            classify_statement(&first_stmt("GRANT SELECT ON t TO r")),
            StatementDispatchKind::Rbac
        );
        assert_eq!(
            classify_statement(&first_stmt("REVOKE SELECT ON t FROM r")),
            StatementDispatchKind::Rbac
        );
    }

    #[test]
    fn classify_statement_covers_more_ddl_and_unsupported_variants() {
        assert_eq!(
            classify_statement(&first_stmt("CREATE VIEW v AS SELECT 1")),
            StatementDispatchKind::Ddl
        );
        assert_eq!(
            classify_statement(&first_stmt("TRUNCATE TABLE t")),
            StatementDispatchKind::Ddl
        );
        assert_eq!(
            classify_statement(&first_stmt("ALTER TABLE t ADD COLUMN c INT")),
            StatementDispatchKind::Ddl
        );
        assert_eq!(
            classify_statement(&first_stmt(
                "SET TRANSACTION ISOLATION LEVEL READ COMMITTED"
            )),
            StatementDispatchKind::UnsupportedSet
        );
        assert_eq!(
            classify_statement(&first_stmt("SET TIME ZONE 'UTC'")),
            StatementDispatchKind::UnsupportedSet
        );
        assert_eq!(
            classify_statement(&first_stmt("BEGIN")),
            StatementDispatchKind::UnsupportedOther
        );
    }

    #[test]
    fn classify_statement_covers_more_ddl_variants() {
        assert_eq!(
            classify_statement(&first_stmt("CREATE SCHEMA s1")),
            StatementDispatchKind::Ddl
        );
        assert_eq!(
            classify_statement(&first_stmt("CREATE SEQUENCE seq1")),
            StatementDispatchKind::Ddl
        );
        assert_eq!(
            classify_statement(&first_stmt("ALTER INDEX idx_t_id RENAME TO idx_t_id2")),
            StatementDispatchKind::Ddl
        );
        assert_eq!(
            classify_statement(&first_stmt("TRUNCATE t")),
            StatementDispatchKind::Ddl
        );
        assert_eq!(
            classify_statement(&first_stmt("DROP TABLE IF EXISTS t")),
            StatementDispatchKind::Ddl
        );
    }

    #[test]
    fn classify_statement_covers_more_query_and_rbac_variants() {
        assert_eq!(
            classify_statement(&first_stmt("SHOW TABLES")),
            StatementDispatchKind::Query
        );
        assert_eq!(
            classify_statement(&first_stmt("EXPLAIN ANALYZE SELECT 1")),
            StatementDispatchKind::Query
        );
        assert_eq!(
            classify_statement(&first_stmt("ALTER ROLE r WITH LOGIN")),
            StatementDispatchKind::Rbac
        );
        assert_eq!(
            classify_statement(&first_stmt("GRANT SELECT ON TABLE t TO r")),
            StatementDispatchKind::Rbac
        );
    }
}
