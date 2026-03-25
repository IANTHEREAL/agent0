//! CREATE, REFRESH, and DROP MATERIALIZED VIEW execution.
//!
//! Implements `execute_create_materialized_view`,
//! `execute_refresh_materialized_view_cmd`, and
//! `execute_drop_materialized_view_cmd` on `Executor`.

use super::super::super::ddl;
use super::super::super::names;
use super::super::super::value_coercion::infer_data_type;
use super::super::super::{parse_sql, ExecuteResult, Session};
use super::super::core::Executor;
use super::{is_unquoted_keyword, parse_object_name, tokenize_non_whitespace};
use crate::model::{ColumnDef, DataType, Row, TableSchema, Value};
use crate::sql::sequences::SequenceSession;
use anyhow::{anyhow, Result};
use sqlparser::ast::{ObjectName, Statement};
use sqlparser::tokenizer::Token;
use std::collections::HashMap;
use tikv_client::Transaction;

// ── Parsing helpers ─────────────────────────────────────────

pub(super) fn parse_refresh_materialized_view_name(sql: &str) -> Result<ObjectName> {
    let tokens = tokenize_non_whitespace(sql)?;
    let mut i = 0usize;

    if !is_unquoted_keyword(
        tokens
            .get(i)
            .ok_or_else(|| anyhow!("Invalid REFRESH MATERIALIZED VIEW syntax"))?,
        "REFRESH",
    ) || !is_unquoted_keyword(
        tokens
            .get(i + 1)
            .ok_or_else(|| anyhow!("Invalid REFRESH MATERIALIZED VIEW syntax"))?,
        "MATERIALIZED",
    ) || !is_unquoted_keyword(
        tokens
            .get(i + 2)
            .ok_or_else(|| anyhow!("Invalid REFRESH MATERIALIZED VIEW syntax"))?,
        "VIEW",
    ) {
        return Err(anyhow!("Invalid REFRESH MATERIALIZED VIEW syntax"));
    }
    i += 3;

    if tokens
        .get(i)
        .is_some_and(|t| is_unquoted_keyword(t, "CONCURRENTLY"))
    {
        i += 1;
    }

    if !matches!(tokens.get(i), Some(Token::Word(_))) {
        return Err(anyhow!("Missing view name"));
    }
    let (name, _) = parse_object_name(tokens.get(i..).unwrap_or_default())?;
    Ok(name)
}

/// Parsed result of a `DROP MATERIALIZED VIEW` statement.
pub(super) struct DropMaterializedViewParsed {
    pub names: Vec<ObjectName>,
    pub if_exists: bool,
    pub cascade: bool,
}

pub(super) fn parse_drop_materialized_view(sql: &str) -> Result<DropMaterializedViewParsed> {
    let tokens = tokenize_non_whitespace(sql)?;
    let mut i = 0usize;

    if !is_unquoted_keyword(
        tokens
            .get(i)
            .ok_or_else(|| anyhow!("Invalid DROP MATERIALIZED VIEW syntax"))?,
        "DROP",
    ) || !is_unquoted_keyword(
        tokens
            .get(i + 1)
            .ok_or_else(|| anyhow!("Invalid DROP MATERIALIZED VIEW syntax"))?,
        "MATERIALIZED",
    ) || !is_unquoted_keyword(
        tokens
            .get(i + 2)
            .ok_or_else(|| anyhow!("Invalid DROP MATERIALIZED VIEW syntax"))?,
        "VIEW",
    ) {
        return Err(anyhow!("Invalid DROP MATERIALIZED VIEW syntax"));
    }
    i += 3;

    let mut if_exists = false;
    if tokens.get(i).is_some_and(|t| is_unquoted_keyword(t, "IF"))
        && tokens
            .get(i + 1)
            .is_some_and(|t| is_unquoted_keyword(t, "EXISTS"))
    {
        if_exists = true;
        i += 2;
    }

    if !matches!(tokens.get(i), Some(Token::Word(_))) {
        return Err(anyhow!("Missing view name"));
    }

    let mut names = Vec::new();
    let (name, consumed) = parse_object_name(tokens.get(i..).unwrap_or_default())?;
    names.push(name);
    i += consumed;

    while matches!(tokens.get(i), Some(Token::Comma)) {
        i += 1;
        if !matches!(tokens.get(i), Some(Token::Word(_))) {
            return Err(anyhow!("Missing view name"));
        }
        let (name, consumed) = parse_object_name(tokens.get(i..).unwrap_or_default())?;
        names.push(name);
        i += consumed;
    }

    let mut cascade = false;
    if tokens
        .get(i)
        .is_some_and(|t| is_unquoted_keyword(t, "CASCADE"))
    {
        cascade = true;
    }

    Ok(DropMaterializedViewParsed {
        names,
        if_exists,
        cascade,
    })
}

// ── Executor impl ───────────────────────────────────────────

impl Executor {
    pub(crate) async fn execute_create_materialized_view(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut SequenceSession,
        search_path: &[String],
        name: &ObjectName,
        query: &sqlparser::ast::Query,
        or_replace: bool,
        current_role: Option<&str>,
    ) -> Result<ExecuteResult> {
        let result = self
            .execute_query_with_ctes(
                txn,
                db_id,
                sequence_values,
                search_path,
                query,
                &HashMap::new(),
                current_role,
            )
            .await?;
        let (columns, rows) = match result {
            ExecuteResult::Select {
                columns,
                column_types: _,
                rows,
                timezone: _,
            } => (columns, rows),
            _ => return Err(anyhow!("Materialized view must be a SELECT query")),
        };

        let resolved = names::resolve_ddl_object_name(name, search_path)?;
        if !self
            .store()
            .schema_exists(txn, db_id, &resolved.schema)
            .await?
        {
            return Err(anyhow!("schema '{}' does not exist", resolved.schema));
        }
        let view_name = resolved.full;

        let table_id = self.store().next_table_id(txn, db_id).await?;
        let mut col_defs: Vec<ColumnDef> = vec![ColumnDef {
            name: "_mv_rowid".to_string(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: true,
            default_expr: None,
            generation_expr: None,
            generation_expr_authorized_by: None,
            collation: None,
            is_serial: true,
            unique: true,
            is_dropped: false,
        }];
        col_defs.extend(columns.iter().enumerate().map(|(i, col_name)| {
            let data_type = if rows.is_empty() {
                DataType::Text
            } else {
                infer_data_type(&rows[0].values[i])
            };
            ColumnDef {
                name: col_name.clone(),
                data_type,
                nullable: true,
                primary_key: false,
                default_expr: None,
                generation_expr: None,
                generation_expr_authorized_by: None,
                collation: None,
                is_serial: false,
                unique: false,
                is_dropped: false,
            }
        }));

        let rows_with_rowid: Vec<Row> = rows
            .into_iter()
            .enumerate()
            .map(|(i, mut row)| {
                let mut values = vec![Value::Int64((i + 1) as i64)];
                values.append(&mut row.values);
                Row::new(values)
            })
            .collect();

        let schema = TableSchema {
            table_id,
            name: view_name.clone(),
            columns: col_defs,
            version: 1,
            pk_constraint_name: Some(format!(
                "{}_pkey",
                view_name.rsplit('.').next().unwrap_or(&view_name)
            )),
            pk_indices: vec![0],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: "postgres".to_string(),
            rls_enabled: false,
            rls_force: false,
            from_alias: None,
        };

        ddl::execute_create_materialized_view(
            &self.store(),
            txn,
            db_id,
            search_path,
            name,
            query,
            or_replace,
            schema,
            rows_with_rowid,
            sequence_values,
        )
        .await
    }

    pub(crate) async fn execute_refresh_materialized_view_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let view_obj = parse_refresh_materialized_view_name(sql)?;
        let view_name_for_error = view_obj.to_string();

        // Detect CONCURRENTLY keyword
        let concurrently = sql.to_uppercase().contains("CONCURRENTLY");

        if concurrently {
            // Enqueue as BgDdl task and return immediately
            let is_autocommit = !session.is_in_transaction();
            if is_autocommit {
                session.begin().await?;
            }

            let result = async {
                let db_id = session.current_database_id();
                let (txn, _, search_path) = session
                    .get_mut_txn_sequence_values_and_search_path()
                    .expect("Transaction must be active");

                // Resolve view name to validate it exists
                let resolved = names::resolve_existing_materialized_view_name(
                    self.store().as_ref(),
                    txn,
                    db_id,
                    &view_obj,
                    search_path,
                )
                .await?
                .ok_or_else(|| {
                    anyhow!("Materialized view '{}' does not exist", view_name_for_error)
                })?;
                let view_full_name = resolved.full;

                // CONCURRENTLY requires the worker to process BgDdl.
                let system_store = crate::worker::get_system_store().ok_or_else(|| {
                    anyhow!(
                        "Cannot REFRESH MATERIALIZED VIEW CONCURRENTLY: worker subsystem is \
                         disabled (DB9_WORKER_ENABLED=false). Background refresh requires the \
                         worker engine. Enable the worker or use REFRESH without CONCURRENTLY."
                    )
                })?;
                // Enqueue BgDdl task
                {
                    let keyspace = self.tenant_keyspace().to_string();
                    let username = session
                        .current_user()
                        .map(|u| u.to_string())
                        .unwrap_or_default();

                    // Create command: REFRESH MATERIALIZED VIEW view_name (without CONCURRENTLY)
                    let command = format!("REFRESH MATERIALIZED VIEW {}", view_full_name);

                    let entry = crate::worker::types::TaskQueueEntry::new(
                        keyspace.clone(),
                        db_id,
                        0i64, // task_id not used for REFRESH MV
                        crate::worker::types::TaskType::BgDdl,
                        command,
                        username,
                        128, // default priority
                    );

                    let now_ms = chrono::Utc::now().timestamp_millis();
                    let mut sys_txn = system_store.begin().await?;
                    system_store
                        .put_worker_queue_entry(&mut sys_txn, &entry, now_ms)
                        .await?;
                    system_store
                        .update_registry_task_types(
                            &mut sys_txn,
                            &keyspace,
                            db_id,
                            crate::worker::types::TASK_TYPE_BG_DDL,
                            0,
                        )
                        .await?;
                    sys_txn.commit().await?;
                    crate::worker::wake_worker();
                }

                Ok(ExecuteResult::RefreshMaterializedView {
                    view_name: view_full_name,
                })
            }
            .await;

            if is_autocommit {
                if result.is_ok() {
                    session.commit().await?;
                } else {
                    session.rollback().await?;
                }
            }

            return result;
        }

        // Non-concurrent path: execute synchronously
        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }

        // Extract role before mutable borrow of session for the transaction.
        let current_role = session.current_user().map(|u| u.to_string());

        let result = async {
            let db_id = session.current_database_id();
            let (txn, sequence_values, search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");

            let resolved = names::resolve_existing_materialized_view_name(
                self.store().as_ref(),
                txn,
                db_id,
                &view_obj,
                search_path,
            )
            .await?
            .ok_or_else(|| anyhow!("Materialized view '{}' does not exist", view_name_for_error))?;
            let view_full_name = resolved.full;

            let query_str = self
                .store()
                .get_materialized_view(txn, db_id, &view_full_name)
                .await?
                .ok_or_else(|| anyhow!("Materialized view '{}' does not exist", view_full_name))?
                .query;

            let ast = parse_sql(&query_str)?;
            let query = match ast.into_iter().next() {
                Some(Statement::Query(q)) => q,
                _ => return Err(anyhow!("Invalid materialized view query")),
            };

            let result = self
                .execute_query_with_ctes(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    &query,
                    &HashMap::new(),
                    current_role.as_deref(),
                )
                .await?;
            let rows = match result {
                ExecuteResult::Select { rows, .. } => rows,
                _ => return Err(anyhow!("Materialized view must be a SELECT query")),
            };

            let rows_with_rowid: Vec<Row> = rows
                .into_iter()
                .enumerate()
                .map(|(i, mut row)| {
                    let mut values = vec![Value::Int64((i + 1) as i64)];
                    values.append(&mut row.values);
                    Row::new(values)
                })
                .collect();

            ddl::execute_refresh_materialized_view(
                &self.store(),
                txn,
                db_id,
                &view_full_name,
                rows_with_rowid,
            )
            .await
        }
        .await;

        if is_autocommit {
            if result.is_ok() {
                session.commit().await?;
            } else {
                session.rollback().await?;
            }
        }

        result
    }

    pub(crate) async fn execute_drop_materialized_view_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let DropMaterializedViewParsed {
            names,
            if_exists,
            cascade,
        } = parse_drop_materialized_view(sql)?;

        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }

        let result = async {
            let db_id = session.current_database_id();
            let (txn, sequence_values, search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");
            ddl::execute_drop_materialized_view(
                &self.store(),
                txn,
                db_id,
                search_path,
                &names,
                if_exists,
                cascade,
                sequence_values,
            )
            .await
        }
        .await;

        if is_autocommit {
            if result.is_ok() {
                session.commit().await?;
            } else {
                session.rollback().await?;
            }
        }

        result
    }
}
