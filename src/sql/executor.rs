//! SQL executor

use super::ddl;
use super::dml;
use super::explain;
use super::helpers::{
    collect_having_agg_funcs, cte_is_recursive, dedup_rows, distinct_on_rows, eval_default_expr,
    eval_having_expr, fill_row_defaults, get_select_item_name, get_skip_reason,
    get_unsupported_reason, infer_expr_type, parse_value_for_copy, query_has_outer_reference,
    set_expr_references_table, substitute_outer_values_in_query, value_to_sql_expr, AggExpr,
};
use super::planner::{self, ScanType};
use super::query;
use super::rbac;
use super::window::{compute_window_functions, extract_window_functions};
use super::{
    expr::{eval_expr, eval_expr_join, JoinContext},
    parse_sql, Aggregator, ExecuteResult, Session,
};
use crate::auth::AuthManager;
use crate::storage::TikvStore;
use crate::types::{ColumnDef, DataType, Row, TableSchema, Value};
use anyhow::{anyhow, Result};
use sqlparser::ast::{
    AlterTableOperation, Assignment, BinaryOperator, ColumnDef as SqlColumnDef, Distinct, Expr,
    FunctionArg, FunctionArgExpr, GroupByExpr, Ident, LockType, ObjectName, OnInsert, OrderByExpr,
    Query, SelectItem, SetExpr, SetOperator, SetQuantifier, Statement, TableFactor,
    Value as SqlValue, Values,
};

use std::collections::HashMap;
use std::sync::Arc;
use tikv_client::Transaction;
use tracing::debug;

fn get_full_table_name(name: &ObjectName) -> String {
    name.0
        .iter()
        .map(|i| i.value.clone())
        .collect::<Vec<_>>()
        .join(".")
}

fn get_simple_table_name(name: &ObjectName) -> String {
    name.0.last().map(|i| i.value.clone()).unwrap_or_default()
}

pub struct Executor {
    store: Arc<TikvStore>,
    auth_manager: AuthManager,
}

impl Executor {
    pub fn new(store: Arc<TikvStore>) -> Self {
        Self {
            store,
            auth_manager: AuthManager::new(),
        }
    }

    pub fn store(&self) -> Arc<TikvStore> {
        self.store.clone()
    }

    #[allow(dead_code)]
    pub fn auth_manager(&self) -> &AuthManager {
        &self.auth_manager
    }

    /// Execute a SQL statement string using the provided session
    /// Supports multiple statements separated by semicolons (e.g., "BEGIN; UPDATE...; COMMIT;")
    pub async fn execute(&self, session: &mut Session, sql: &str) -> Result<ExecuteResult> {
        let sql_upper = sql.trim().to_uppercase();
        if let Some(reason) = get_skip_reason(&sql_upper) {
            return Ok(ExecuteResult::Skipped { message: reason });
        }

        if sql_upper.starts_with("REFRESH MATERIALIZED VIEW") {
            return self
                .execute_refresh_materialized_view_cmd(session, sql)
                .await;
        }

        if sql_upper.starts_with("DROP MATERIALIZED VIEW") {
            return self.execute_drop_materialized_view_cmd(session, sql).await;
        }

        if sql_upper.starts_with("CALL ") {
            return self.execute_call_cmd(session, sql).await;
        }

        if sql_upper.starts_with("DROP PROCEDURE") {
            return self.execute_drop_procedure_cmd(session, sql).await;
        }

        if sql_upper.starts_with("CREATE PROCEDURE") {
            return self.execute_create_procedure_cmd(session, sql).await;
        }

        let statements = match parse_sql(sql) {
            Ok(stmts) => stmts,
            Err(e) => {
                if let Some(reason) = get_unsupported_reason(&sql_upper) {
                    return Ok(ExecuteResult::Skipped { message: reason });
                }
                return Err(e);
            }
        };

        if statements.is_empty() {
            return Ok(ExecuteResult::Empty);
        }

        // Execute all statements in order, returning the result of the last one
        let mut last_result = ExecuteResult::Empty;

        for stmt in &statements {
            debug!("Executing statement: {:?}", stmt);

            last_result = match stmt {
                // Transaction Control
                Statement::StartTransaction { .. } => {
                    session.begin().await?;
                    ExecuteResult::Empty
                }
                Statement::Commit { .. } => {
                    session.commit().await?;
                    ExecuteResult::Empty
                }
                Statement::Rollback { .. } => {
                    session.rollback().await?;
                    ExecuteResult::Empty
                }
                // DDL/DML - delegated to session transaction management
                _ => {
                    let is_autocommit = !session.is_in_transaction();

                    if is_autocommit {
                        session.begin().await?;
                    }

                    let res = async {
                        let txn = session.get_mut_txn().expect("Transaction must be active");
                        self.execute_statement_on_txn(txn, stmt).await
                    }
                    .await;

                    if is_autocommit {
                        if res.is_ok() {
                            session.commit().await?;
                        } else {
                            session.rollback().await?;
                        }
                    }

                    res?
                }
            };
        }

        Ok(last_result)
    }

    /// Execute a parsed SQL statement on a given transaction
    pub(crate) async fn execute_statement_on_txn(
        &self,
        txn: &mut Transaction,
        stmt: &Statement,
    ) -> Result<ExecuteResult> {
        match stmt {
            Statement::CreateTable {
                name,
                columns,
                constraints,
                if_not_exists,
                query,
                temporary,
                ..
            } => {
                if let Some(q) = query {
                    self.execute_create_table_as(txn, name, q, columns, *if_not_exists, *temporary)
                        .await
                } else {
                    ddl::execute_create_table(
                        &self.store,
                        txn,
                        name,
                        columns,
                        constraints,
                        *if_not_exists,
                    )
                    .await
                }
            }
            Statement::CreateIndex {
                name,
                table_name,
                columns,
                unique,
                if_not_exists,
                ..
            } => {
                let index_name = name
                    .as_ref()
                    .ok_or_else(|| anyhow!("Index name required"))?;
                let idx_name_str = index_name.0.last().unwrap().value.as_str();
                self.execute_create_index(
                    txn,
                    idx_name_str,
                    table_name,
                    columns,
                    *unique,
                    *if_not_exists,
                )
                .await
            }
            Statement::Drop {
                object_type,
                names,
                if_exists,
                ..
            } => {
                use sqlparser::ast::ObjectType;
                match object_type {
                    ObjectType::Table => {
                        ddl::execute_drop_table(&self.store, txn, names, *if_exists).await
                    }
                    ObjectType::View => {
                        ddl::execute_drop_view(&self.store, txn, names, *if_exists).await
                    }
                    ObjectType::Index => self.execute_drop_index(txn, names, *if_exists).await,
                    ObjectType::Role => {
                        rbac::execute_drop_role(&self.auth_manager, txn, names, *if_exists).await
                    }
                    ObjectType::Sequence => Ok(ExecuteResult::Empty),
                    _ => Ok(ExecuteResult::Empty),
                }
            }
            Statement::Truncate { table_name, .. } => {
                ddl::execute_truncate(&self.store, txn, table_name).await
            }
            Statement::AlterTable {
                name, operations, ..
            } => {
                for op in operations {
                    self.execute_alter_table(txn, name, op).await?;
                }
                let table_name = name.0.last().unwrap().value.clone();
                Ok(ExecuteResult::AlterTable { table_name })
            }
            Statement::Insert {
                table_name,
                columns,
                source,
                returning,
                on,
                ..
            } => {
                self.execute_insert(txn, table_name, columns, source, returning, on)
                    .await
            }
            Statement::Delete {
                from,
                selection,
                returning,
                ..
            } => self.execute_delete(txn, from, selection, returning).await,
            Statement::Update {
                table,
                assignments,
                from,
                selection,
                returning,
                ..
            } => {
                self.execute_update(txn, table, assignments, from, selection, returning)
                    .await
            }
            Statement::Query(query) => self.execute_query(txn, query).await,
            Statement::ShowTables { .. } => self.execute_show_tables(txn).await,
            Statement::SetVariable { .. }
            | Statement::SetTimeZone { .. }
            | Statement::SetNames { .. }
            | Statement::SetTransaction { .. } => Ok(ExecuteResult::Empty),
            Statement::CreateType { .. } | Statement::CreateFunction { .. } => {
                Ok(ExecuteResult::Empty)
            }
            Statement::CreateProcedure {
                name, params, body, ..
            } => {
                self.execute_create_procedure(txn, name, params.as_deref(), body)
                    .await
            }
            Statement::CreateSequence { .. } => Ok(ExecuteResult::Empty),
            Statement::CreateView {
                name,
                query,
                or_replace,
                materialized,
                ..
            } => {
                if *materialized {
                    self.execute_create_materialized_view(txn, name, query, *or_replace)
                        .await
                } else {
                    ddl::execute_create_view(&self.store, txn, name, query, *or_replace).await
                }
            }
            Statement::AlterIndex { .. } => Ok(ExecuteResult::Empty),
            Statement::CreateRole {
                names,
                if_not_exists,
                login,
                password,
                superuser,
                create_db,
                create_role,
                ..
            } => {
                rbac::execute_create_role(
                    &self.auth_manager,
                    txn,
                    names,
                    *if_not_exists,
                    login,
                    password,
                    superuser,
                    create_db,
                    create_role,
                )
                .await
            }
            Statement::AlterRole { name, operation } => {
                rbac::execute_alter_role(&self.auth_manager, txn, name, operation).await
            }
            Statement::Grant {
                privileges,
                objects,
                grantees,
                with_grant_option,
                ..
            } => {
                rbac::execute_grant(
                    &self.auth_manager,
                    txn,
                    privileges,
                    &Some(objects.clone()),
                    grantees,
                    *with_grant_option,
                )
                .await
            }
            Statement::Revoke {
                privileges,
                objects,
                grantees,
                ..
            } => {
                rbac::execute_revoke(
                    &self.auth_manager,
                    txn,
                    privileges,
                    &Some(objects.clone()),
                    grantees,
                )
                .await
            }
            Statement::Comment { .. } => Ok(ExecuteResult::Empty),
            Statement::Copy { .. } => Ok(ExecuteResult::Empty),
            Statement::Explain {
                statement,
                analyze,
                verbose,
                ..
            } => {
                self.execute_explain(txn, statement, *analyze, *verbose)
                    .await
            }
            _ => Err(anyhow!("Unsupported statement: {:?}", stmt)),
        }
    }

    async fn execute_create_table_as(
        &self,
        txn: &mut Transaction,
        name: &ObjectName,
        query: &Query,
        columns: &[SqlColumnDef],
        if_not_exists: bool,
        _temporary: bool,
    ) -> Result<ExecuteResult> {
        let table_name = name
            .0
            .last()
            .ok_or_else(|| anyhow!("Invalid table name"))?
            .value
            .clone();

        let ctes = self.build_cte_context(txn, query).await?;
        let result = self.execute_query_with_ctes(txn, query, &ctes).await?;

        let (result_cols, result_rows) = match result {
            ExecuteResult::Select {
                columns: cols,
                rows,
                ..
            } => (cols, rows),
            _ => return Err(anyhow!("CREATE TABLE AS requires a SELECT query")),
        };

        ddl::create_table_from_query_result(
            &self.store,
            txn,
            &table_name,
            if_not_exists,
            result_cols,
            result_rows,
            columns,
        )
        .await
    }

    async fn create_table_from_result(
        &self,
        txn: &mut Transaction,
        target_name: &ObjectName,
        result: ExecuteResult,
    ) -> Result<ExecuteResult> {
        let table_name = target_name
            .0
            .last()
            .ok_or_else(|| anyhow!("Invalid table name"))?
            .value
            .clone();

        let (result_cols, result_rows) = match result {
            ExecuteResult::Select {
                columns: cols,
                rows,
                ..
            } => (cols, rows),
            _ => return Err(anyhow!("SELECT INTO requires a SELECT query")),
        };

        ddl::create_table_from_select_into(&self.store, txn, &table_name, result_cols, result_rows)
            .await
    }

    async fn execute_create_index(
        &self,
        txn: &mut Transaction,
        idx_name: &str,
        table_name: &ObjectName,
        columns: &[OrderByExpr],
        unique: bool,
        if_not_exists: bool,
    ) -> Result<ExecuteResult> {
        let tbl_name = table_name.0.last().unwrap().value.clone();
        let schema = self
            .store
            .get_schema(txn, &tbl_name)
            .await?
            .ok_or_else(|| anyhow!("Table not found"))?;
        let rows = self.scan_and_fill(txn, &tbl_name, &schema).await?;
        ddl::execute_create_index(
            &self.store,
            txn,
            idx_name,
            table_name,
            columns,
            unique,
            if_not_exists,
            rows,
        )
        .await
    }

    async fn execute_drop_index(
        &self,
        txn: &mut Transaction,
        names: &[ObjectName],
        if_exists: bool,
    ) -> Result<ExecuteResult> {
        let mut last_index = String::new();
        for name in names {
            let idx_name = name.0.last().unwrap().value.clone();
            let mut found = false;

            let tables = self.store.list_tables(txn).await?;
            for table_name in tables {
                let mut schema = match self.store.get_schema(txn, &table_name).await? {
                    Some(s) => s,
                    None => continue,
                };

                let rows = self.scan_and_fill(txn, &table_name, &schema).await?;
                if let Some(dropped) = ddl::execute_drop_index(
                    &self.store,
                    txn,
                    &idx_name,
                    &mut schema,
                    &table_name,
                    rows,
                )
                .await?
                {
                    found = true;
                    last_index = dropped;
                    break;
                }
            }

            if !found && !if_exists {
                return Err(anyhow!("Index '{}' does not exist", idx_name));
            }
        }
        Ok(ExecuteResult::DropIndex {
            index_name: last_index,
        })
    }

    async fn execute_alter_table(
        &self,
        txn: &mut Transaction,
        name: &ObjectName,
        operation: &AlterTableOperation,
    ) -> Result<ExecuteResult> {
        let t = name.0.last().unwrap().value.clone();
        let schema = self
            .store
            .get_schema(txn, &t)
            .await?
            .ok_or_else(|| anyhow!("Table '{}' does not exist", t))?;
        let rows = self.scan_and_fill(txn, &t, &schema).await?;
        ddl::execute_alter_table(&self.store, txn, name, operation, rows).await
    }

    async fn execute_insert(
        &self,
        txn: &mut Transaction,
        table_name: &ObjectName,
        columns: &[Ident],
        source: &Option<Box<Query>>,
        returning: &Option<Vec<SelectItem>>,
        on_conflict: &Option<OnInsert>,
    ) -> Result<ExecuteResult> {
        let t = table_name.0.last().unwrap().value.clone();
        let schema = self
            .store
            .get_schema(txn, &t)
            .await?
            .ok_or_else(|| anyhow!("Table '{}' does not exist", t))?;
        let source = source
            .as_ref()
            .ok_or_else(|| anyhow!("INSERT requires VALUES"))?;
        let values = match &*source.body {
            SetExpr::Values(Values { rows, .. }) => rows,
            _ => return Err(anyhow!("Only VALUES supported")),
        };

        let mut affected = 0;
        let mut ret_rows = Vec::new();
        let ret_cols = dml::build_returning_columns(returning, &schema)?;

        for exprs in values {
            let (mut row_vals, indices) = dml::prepare_insert_row(&schema, columns, exprs)?;
            dml::fill_missing_columns(&self.store, txn, &schema, &mut row_vals, &indices).await?;
            dml::coerce_row_values(&schema, &mut row_vals)?;
            let row = Row::new(row_vals);
            dml::validate_check_constraints(&schema, &row)?;

            let result =
                dml::execute_insert_row(&self.store, txn, &t, &schema, row, on_conflict).await?;
            if let Some(final_row) = result {
                affected += 1;
                if let Some(ret_row) = dml::eval_returning_row(returning, &final_row, &schema)? {
                    ret_rows.push(ret_row);
                }
            }
        }

        if returning.is_some() {
            let column_types = Some(
                ret_cols
                    .iter()
                    .map(|col_name| {
                        schema
                            .columns
                            .iter()
                            .find(|c| c.name.eq_ignore_ascii_case(col_name))
                            .map(|c| c.data_type.clone())
                            .unwrap_or(DataType::Text)
                    })
                    .collect(),
            );
            Ok(ExecuteResult::Select {
                column_types,
                columns: ret_cols,
                rows: ret_rows,
            })
        } else {
            Ok(ExecuteResult::Insert {
                affected_rows: affected,
            })
        }
    }

    async fn execute_delete(
        &self,
        txn: &mut Transaction,
        from: &[sqlparser::ast::TableWithJoins],
        selection: &Option<Expr>,
        returning: &Option<Vec<SelectItem>>,
    ) -> Result<ExecuteResult> {
        let t = match &from[0].relation {
            sqlparser::ast::TableFactor::Table { name, .. } => name.0.last().unwrap().value.clone(),
            _ => return Err(anyhow!("Unsupported")),
        };
        let schema = self
            .store
            .get_schema(txn, &t)
            .await?
            .ok_or_else(|| anyhow!("Table not found"))?;
        if schema.pk_indices.is_empty() {
            return Err(anyhow!("No PK"));
        }
        let resolved_selection = if let Some(sel) = selection {
            Some(self.resolve_subqueries(txn, sel).await?)
        } else {
            None
        };
        let rows = self.scan_and_fill(txn, &t, &schema).await?;
        let mut cnt = 0;
        let mut ret_rows = Vec::new();
        let ret_cols = dml::build_returning_columns(returning, &schema)?;

        for r in rows {
            if let Some(ref e) = resolved_selection {
                if !matches!(eval_expr(e, Some(&r), Some(&schema))?, Value::Boolean(true)) {
                    continue;
                }
            }
            if let Some(ret_row) = dml::eval_returning_row(returning, &r, &schema)? {
                ret_rows.push(ret_row);
            }
            dml::execute_delete_row(&self.store, txn, &t, &schema, &r).await?;
            cnt += 1;
        }

        if returning.is_some() {
            let column_types = Some(
                ret_cols
                    .iter()
                    .map(|col_name| {
                        schema
                            .columns
                            .iter()
                            .find(|c| c.name.eq_ignore_ascii_case(col_name))
                            .map(|c| c.data_type.clone())
                            .unwrap_or(DataType::Text)
                    })
                    .collect(),
            );
            Ok(ExecuteResult::Select {
                column_types,
                columns: ret_cols,
                rows: ret_rows,
            })
        } else {
            Ok(ExecuteResult::Delete { affected_rows: cnt })
        }
    }

    async fn execute_update(
        &self,
        txn: &mut Transaction,
        table: &sqlparser::ast::TableWithJoins,
        assignments: &[Assignment],
        from: &Option<sqlparser::ast::TableWithJoins>,
        selection: &Option<Expr>,
        returning: &Option<Vec<SelectItem>>,
    ) -> Result<ExecuteResult> {
        let t = match &table.relation {
            sqlparser::ast::TableFactor::Table { name, .. } => name.0.last().unwrap().value.clone(),
            _ => return Err(anyhow!("Unsupported")),
        };
        let table_alias = match &table.relation {
            sqlparser::ast::TableFactor::Table { alias, .. } => alias
                .as_ref()
                .map(|a| a.name.value.clone())
                .unwrap_or_else(|| t.clone()),
            _ => t.clone(),
        };
        let schema = self
            .store
            .get_schema(txn, &t)
            .await?
            .ok_or_else(|| anyhow!("Table not found"))?;
        if schema.pk_indices.is_empty() {
            return Err(anyhow!("No PK"));
        }
        let resolved_selection = if let Some(sel) = selection {
            Some(self.resolve_subqueries(txn, sel).await?)
        } else {
            None
        };
        let indices = dml::validate_update_columns(&schema, assignments)?;

        let (from_schema, from_rows, from_alias) = if let Some(from_table) = from {
            let from_name = match &from_table.relation {
                sqlparser::ast::TableFactor::Table { name, .. } => {
                    name.0.last().unwrap().value.clone()
                }
                _ => return Err(anyhow!("Unsupported FROM table")),
            };
            let from_alias_str = match &from_table.relation {
                sqlparser::ast::TableFactor::Table { alias, .. } => alias
                    .as_ref()
                    .map(|a| a.name.value.clone())
                    .unwrap_or_else(|| from_name.clone()),
                _ => from_name.clone(),
            };
            let fs = self
                .store
                .get_schema(txn, &from_name)
                .await?
                .ok_or_else(|| anyhow!("FROM table not found"))?;
            let fr = self.scan_and_fill(txn, &from_name, &fs).await?;
            (Some(fs), Some(fr), Some(from_alias_str))
        } else {
            (None, None, None)
        };

        let rows = self.scan_and_fill(txn, &t, &schema).await?;
        let mut cnt = 0;
        let mut ret_rows = Vec::new();
        let ret_cols = dml::build_returning_columns(returning, &schema)?;

        for r in &rows {
            let matching_from_rows: Vec<&Row> = if let (Some(ref fs), Some(ref fr), Some(ref fa)) =
                (&from_schema, &from_rows, &from_alias)
            {
                let (combined_schema, _, column_offsets) =
                    dml::build_update_join_context(&schema, &table_alias, fs, fa, r, &fr[0]);

                fr.iter()
                    .filter(|from_row| {
                        if let Some(ref sel) = resolved_selection {
                            let mut combined_values = r.values.clone();
                            combined_values.extend(from_row.values.clone());
                            let combined_row = Row::new(combined_values);
                            let ctx = JoinContext {
                                tables: HashMap::new(),
                                column_offsets: column_offsets.clone(),
                                combined_row: &combined_row,
                                combined_schema: &combined_schema,
                            };
                            matches!(eval_expr_join(sel, &ctx), Ok(Value::Boolean(true)))
                        } else {
                            true
                        }
                    })
                    .collect()
            } else {
                if let Some(ref e) = resolved_selection {
                    if !matches!(
                        eval_expr(e, Some(r), Some(&schema)),
                        Ok(Value::Boolean(true))
                    ) {
                        continue;
                    }
                }
                vec![r]
            };

            if matching_from_rows.is_empty() && from.is_some() {
                continue;
            }

            let new_vals = if let (Some(ref fs), Some(ref fa)) = (&from_schema, &from_alias) {
                if let Some(first_from) = matching_from_rows.first() {
                    let (combined_schema, combined_row, _) = dml::build_update_join_context(
                        &schema,
                        &table_alias,
                        fs,
                        fa,
                        r,
                        first_from,
                    );
                    dml::compute_update_values(
                        &schema,
                        r,
                        assignments,
                        &indices,
                        Some((&combined_row, &combined_schema)),
                    )?
                } else {
                    dml::compute_update_values(&schema, r, assignments, &indices, None)?
                }
            } else {
                dml::compute_update_values(&schema, r, assignments, &indices, None)?
            };

            let new_row = Row::new(new_vals);
            dml::validate_check_constraints(&schema, &new_row)?;
            let updated_row =
                dml::execute_update_row(&self.store, txn, &t, &schema, r, new_row).await?;

            if let Some(ret_row) = dml::eval_returning_row(returning, &updated_row, &schema)? {
                ret_rows.push(ret_row);
            }
            cnt += 1;
        }

        if returning.is_some() {
            // Extract column types from schema for RETURNING clause
            let column_types = Some(
                ret_cols
                    .iter()
                    .map(|col_name| {
                        schema
                            .columns
                            .iter()
                            .find(|c| c.name.eq_ignore_ascii_case(col_name))
                            .map(|c| c.data_type.clone())
                            .unwrap_or(DataType::Text)
                    })
                    .collect(),
            );

            Ok(ExecuteResult::Select {
                column_types,
                columns: ret_cols,
                rows: ret_rows,
            })
        } else {
            Ok(ExecuteResult::Update { affected_rows: cnt })
        }
    }

    async fn execute_query(&self, txn: &mut Transaction, query: &Query) -> Result<ExecuteResult> {
        let ctes = self.build_cte_context(txn, query).await?;
        self.execute_query_with_ctes(txn, query, &ctes).await
    }

    async fn build_cte_context(
        &self,
        txn: &mut Transaction,
        query: &Query,
    ) -> Result<HashMap<String, (TableSchema, Vec<Row>)>> {
        let mut ctes: HashMap<String, (TableSchema, Vec<Row>)> = HashMap::new();
        if let Some(with) = &query.with {
            for cte in &with.cte_tables {
                let cte_name = cte.alias.name.value.to_lowercase();

                if with.recursive && cte_is_recursive(&cte.query, &cte_name) {
                    let (schema, rows) = self
                        .execute_recursive_cte(
                            txn,
                            &cte_name,
                            &cte.query,
                            &cte.alias.columns,
                            &ctes,
                        )
                        .await?;
                    ctes.insert(cte_name, (schema, rows));
                } else {
                    let cte_result = self.execute_query_with_ctes(txn, &cte.query, &ctes).await?;
                    match cte_result {
                        ExecuteResult::Select {
                            columns,
                            column_types: _,
                            rows,
                        } => {
                            let col_names: Vec<String> = if cte.alias.columns.is_empty() {
                                columns
                            } else {
                                cte.alias.columns.iter().map(|c| c.value.clone()).collect()
                            };
                            let schema = TableSchema {
                                table_id: 0,
                                name: cte_name.clone(),
                                columns: col_names
                                    .iter()
                                    .map(|n| ColumnDef {
                                        name: n.clone(),
                                        data_type: DataType::Text,
                                        nullable: true,
                                        primary_key: false,
                                        unique: false,
                                        is_serial: false,
                                        default_expr: None,
                                    })
                                    .collect(),
                                pk_indices: vec![],
                                indexes: vec![],
                                version: 1,
                                check_constraints: vec![],
                                foreign_keys: vec![],
                            };
                            ctes.insert(cte_name, (schema, rows));
                        }
                        _ => return Err(anyhow!("CTE must be a SELECT query")),
                    }
                }
            }
        }
        Ok(ctes)
    }

    async fn execute_recursive_cte(
        &self,
        txn: &mut Transaction,
        cte_name: &str,
        query: &Query,
        alias_columns: &[Ident],
        existing_ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Result<(TableSchema, Vec<Row>)> {
        let (base_expr, recursive_expr, is_union_all) = match &*query.body {
            SetExpr::SetOperation {
                op: SetOperator::Union,
                set_quantifier,
                left,
                right,
            } => {
                let is_all = matches!(set_quantifier, SetQuantifier::All);
                if set_expr_references_table(left, cte_name) {
                    (right.clone(), left.clone(), is_all)
                } else {
                    (left.clone(), right.clone(), is_all)
                }
            }
            _ => return Err(anyhow!("Recursive CTE must use UNION or UNION ALL")),
        };

        let base_query = Query {
            with: None,
            body: base_expr,
            order_by: vec![],
            limit: None,
            offset: None,
            fetch: None,
            locks: vec![],
            limit_by: vec![],
            for_clause: None,
        };
        let base_result = self
            .execute_query_with_ctes(txn, &base_query, existing_ctes)
            .await?;
        let (columns, mut all_rows) = match base_result {
            ExecuteResult::Select {
                columns,
                column_types: _,
                rows,
            } => (columns, rows),
            _ => return Err(anyhow!("Recursive CTE base must be SELECT")),
        };

        let col_names: Vec<String> = if alias_columns.is_empty() {
            columns
        } else {
            alias_columns.iter().map(|c| c.value.clone()).collect()
        };
        let schema = TableSchema {
            table_id: 0,
            name: cte_name.to_string(),
            columns: col_names
                .iter()
                .map(|n| ColumnDef {
                    name: n.clone(),
                    data_type: DataType::Text,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                })
                .collect(),
            pk_indices: vec![],
            indexes: vec![],
            version: 1,
            check_constraints: vec![],
            foreign_keys: vec![],
        };

        let mut working_table = all_rows.clone();
        let max_iterations = 1000;
        let mut iteration = 0;

        while !working_table.is_empty() && iteration < max_iterations {
            iteration += 1;

            let mut temp_ctes = existing_ctes.clone();
            temp_ctes.insert(
                cte_name.to_string(),
                (schema.clone(), working_table.clone()),
            );

            let recursive_query = Query {
                with: None,
                body: recursive_expr.clone(),
                order_by: vec![],
                limit: None,
                offset: None,
                fetch: None,
                locks: vec![],
                limit_by: vec![],
                for_clause: None,
            };
            let recursive_result = self
                .execute_query_with_ctes(txn, &recursive_query, &temp_ctes)
                .await?;

            let new_rows = match recursive_result {
                ExecuteResult::Select { rows, .. } => rows,
                _ => return Err(anyhow!("Recursive CTE iteration must be SELECT")),
            };

            if new_rows.is_empty() {
                break;
            }

            if is_union_all {
                all_rows.extend(new_rows.clone());
                working_table = new_rows;
            } else {
                let mut unique_new_rows = Vec::new();
                for row in new_rows {
                    if !all_rows.contains(&row) {
                        unique_new_rows.push(row.clone());
                        all_rows.push(row);
                    }
                }
                working_table = unique_new_rows;
            }
        }

        Ok((schema, all_rows))
    }

    pub(crate) async fn execute_query_with_ctes(
        &self,
        txn: &mut Transaction,
        query: &Query,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Result<ExecuteResult> {
        // Handle UNION/INTERSECT/EXCEPT
        if let SetExpr::SetOperation {
            op,
            set_quantifier,
            left,
            right,
        } = &*query.body
        {
            return self
                .execute_set_operation(txn, op, set_quantifier, left, right, ctes)
                .await;
        }

        let select = match &*query.body {
            SetExpr::Select(s) => s,
            _ => return Err(anyhow!("Only SELECT supported")),
        };

        let select_into_target = select
            .into
            .as_ref()
            .map(|into| (into.name.clone(), into.temporary));

        if select.from.is_empty() {
            let result = self.execute_tableless_query(txn, select).await?;
            if let Some((target_name, _temp)) = select_into_target {
                return self
                    .create_table_from_result(txn, &target_name, result)
                    .await;
            }
            return Ok(result);
        }

        let has_joins = !select.from[0].joins.is_empty() || select.from.len() > 1;

        if has_joins {
            let result = self
                .execute_join_query_with_ctes(txn, query, select, ctes)
                .await?;
            if let Some((target_name, _temp)) = select_into_target {
                return self
                    .create_table_from_result(txn, &target_name, result)
                    .await;
            }
            return Ok(result);
        }

        let (t, outer_alias, schema, all_rows_base, is_virtual) = match &select.from[0].relation {
            TableFactor::Table { name, alias, .. } => {
                let full_name = get_full_table_name(name);
                let simple_name = get_simple_table_name(name);
                let lookup_name =
                    if super::information_schema::get_information_schema_schema(&full_name)
                        .is_some()
                    {
                        full_name.clone()
                    } else {
                        simple_name.clone()
                    };
                let t_lower = lookup_name.to_lowercase();
                let (schema, rows) = self.get_table_data(txn, &lookup_name, ctes).await?;
                let is_virtual = ctes.contains_key(&t_lower)
                    || super::information_schema::get_information_schema_schema(&t_lower).is_some()
                    || self.store.get_view(txn, &t_lower).await?.is_some();
                let alias_str = alias
                    .as_ref()
                    .map(|a| a.name.value.clone())
                    .unwrap_or_else(|| simple_name.clone());
                (simple_name, alias_str, schema, rows, is_virtual)
            }
            TableFactor::Derived {
                subquery, alias, ..
            } => {
                let alias_name = alias
                    .as_ref()
                    .map(|a| a.name.value.clone())
                    .unwrap_or_else(|| "subquery".to_string());
                let (schema, rows) = self
                    .execute_derived_table(txn, subquery, &alias_name, ctes)
                    .await?;
                (alias_name.clone(), alias_name, schema, rows, true)
            }
            _ => return Err(anyhow!("Unsupported table")),
        };

        let has_correlated_exists = select
            .selection
            .as_ref()
            .map(|sel| self.expr_has_correlated_exists(sel, &outer_alias))
            .unwrap_or(false);

        let resolved_selection = if let Some(sel) = &select.selection {
            if has_correlated_exists {
                Some(sel.clone())
            } else {
                Some(self.resolve_subqueries(txn, sel).await?)
            }
        } else {
            None
        };

        let resolved_projection = self
            .resolve_projection_subqueries_with_outer_context(txn, &select.projection, &outer_alias)
            .await?;

        let all_rows = if is_virtual {
            all_rows_base
        } else {
            let mut index_scan_rows = None;
            if let Some(ref sel) = resolved_selection {
                let predicates = planner::analyze_predicates(sel);
                let estimated_rows = all_rows_base.len().max(100);
                let access_path =
                    planner::choose_best_access_path(&schema, &predicates, estimated_rows);

                match access_path.scan_type {
                    ScanType::IndexScan {
                        index_id,
                        ref index_name,
                        ref values,
                        ..
                    } => {
                        let index = schema.indexes.iter().find(|i| i.id == index_id);
                        if let Some(idx) = index {
                            debug!(
                                "Using Index Scan on {} (cost: {:.2})",
                                index_name, access_path.cost
                            );
                            let pks = self
                                .store
                                .scan_index(txn, schema.table_id, idx.id, values, idx.unique)
                                .await?;
                            if !pks.is_empty() {
                                let mut rows = self
                                    .store
                                    .batch_get_rows(txn, schema.table_id, pks.clone(), &schema)
                                    .await?;
                                if !rows.is_empty() {
                                    for r in &mut rows {
                                        fill_row_defaults(r, &schema)?;
                                    }
                                    index_scan_rows = Some(rows);
                                }
                            }
                        }
                    }
                    ScanType::IndexRangeScan {
                        index_id,
                        ref index_name,
                        ref prefix_values,
                        ..
                    } => {
                        let index = schema.indexes.iter().find(|i| i.id == index_id);
                        if let Some(idx) = index {
                            debug!(
                                "Using Index Range Scan on {} with {} prefix columns (cost: {:.2})",
                                index_name,
                                prefix_values.len(),
                                access_path.cost
                            );
                            let pks = self
                                .store
                                .scan_index(txn, schema.table_id, idx.id, prefix_values, idx.unique)
                                .await?;
                            if !pks.is_empty() {
                                let mut rows = self
                                    .store
                                    .batch_get_rows(txn, schema.table_id, pks.clone(), &schema)
                                    .await?;
                                if !rows.is_empty() {
                                    for r in &mut rows {
                                        fill_row_defaults(r, &schema)?;
                                    }
                                    index_scan_rows = Some(rows);
                                }
                            }
                        }
                    }
                    ScanType::FullTableScan => {
                        debug!("Using Full Table Scan (cost: {:.2})", access_path.cost);
                    }
                }
            }
            index_scan_rows.unwrap_or(all_rows_base)
        };

        let filtered_rows = if let Some(ref sel) = resolved_selection {
            let mut v = Vec::new();
            if has_correlated_exists {
                for r in all_rows {
                    let result = self
                        .eval_selection_with_correlated_exists(txn, sel, &outer_alias, &schema, &r)
                        .await?;
                    if matches!(result, Value::Boolean(true)) {
                        v.push(r);
                    }
                }
            } else {
                for r in all_rows {
                    if matches!(
                        eval_expr(sel, Some(&r), Some(&schema))?,
                        Value::Boolean(true)
                    ) {
                        v.push(r);
                    }
                }
            }
            v
        } else {
            all_rows
        };

        let has_for_update = query
            .locks
            .iter()
            .any(|l| matches!(l.lock_type, LockType::Update));
        if has_for_update && !filtered_rows.is_empty() {
            self.store.lock_rows(txn, &t, &filtered_rows).await?;
        }

        let group_keys_exprs = match &select.group_by {
            GroupByExpr::Expressions(exprs) => exprs,
            GroupByExpr::All => return Err(anyhow!("GROUP BY ALL not supported")),
        };

        let window_funcs = extract_window_functions(&select.projection);

        let mut agg_funcs: Vec<(usize, AggExpr)> = Vec::new();
        for (i, item) in select.projection.iter().enumerate() {
            match item {
                SelectItem::UnnamedExpr(Expr::Function(f))
                | SelectItem::ExprWithAlias {
                    expr: Expr::Function(f),
                    ..
                } => {
                    if f.over.is_none() {
                        let func_name = f
                            .name
                            .0
                            .last()
                            .map(|n| n.value.to_uppercase())
                            .unwrap_or_default();
                        if matches!(
                            func_name.as_str(),
                            "COUNT" | "SUM" | "AVG" | "MIN" | "MAX" | "STRING_AGG" | "ARRAY_AGG"
                        ) {
                            agg_funcs.push((i, AggExpr::Function(f.clone())));
                        }
                    }
                }
                SelectItem::UnnamedExpr(Expr::ArrayAgg(arr))
                | SelectItem::ExprWithAlias {
                    expr: Expr::ArrayAgg(arr),
                    ..
                } => {
                    agg_funcs.push((i, AggExpr::ArrayAgg(arr.clone())));
                }
                _ => {}
            }
        }

        if let Some(having_expr) = &select.having {
            let extra_start = select.projection.len();
            collect_having_agg_funcs(having_expr, &mut agg_funcs, extra_start);
        }

        let is_agg = !group_keys_exprs.is_empty() || !agg_funcs.is_empty();

        if is_agg {
            let mut groups: HashMap<Vec<u8>, Vec<Aggregator>> = HashMap::new();
            let mut group_rows: HashMap<Vec<u8>, Row> = HashMap::new();

            for row in filtered_rows {
                let mut key = Vec::new();
                for expr in group_keys_exprs {
                    key.push(eval_expr(expr, Some(&row), Some(&schema))?);
                }
                let key_bytes = bincode::serialize(&key).unwrap();

                if !groups.contains_key(&key_bytes) {
                    let mut aggs = Vec::new();
                    for (_, agg_expr) in &agg_funcs {
                        match agg_expr {
                            AggExpr::Function(f) => {
                                let name = f.name.0.last().unwrap().value.to_uppercase();
                                if name == "STRING_AGG" {
                                    let delimiter = if f.args.len() >= 2 {
                                        match &f.args[1] {
                                            FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                                                match eval_expr(e, Some(&row), Some(&schema))? {
                                                    Value::Text(s) => s,
                                                    _ => ",".to_string(),
                                                }
                                            }
                                            _ => ",".to_string(),
                                        }
                                    } else {
                                        ",".to_string()
                                    };
                                    aggs.push(Aggregator::new_string_agg(delimiter));
                                } else {
                                    aggs.push(Aggregator::new(&name)?);
                                }
                            }
                            AggExpr::ArrayAgg(_) => {
                                aggs.push(Aggregator::new_array_agg());
                            }
                        }
                    }
                    groups.insert(key_bytes.clone(), aggs);
                    group_rows.insert(key_bytes.clone(), row.clone());
                }

                let aggs = groups.get_mut(&key_bytes).unwrap();
                for (agg_idx, (_, agg_expr)) in agg_funcs.iter().enumerate() {
                    let (filter_expr, arg_expr) = match agg_expr {
                        AggExpr::Function(f) => {
                            let filter = f.filter.as_ref().map(|e| e.as_ref());
                            let arg = if f.args.is_empty() {
                                None
                            } else {
                                match &f.args[0] {
                                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                    FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => None,
                                    _ => return Err(anyhow!("Unsupported arg")),
                                }
                            };
                            (filter, arg)
                        }
                        AggExpr::ArrayAgg(arr) => (None, Some(arr.expr.as_ref())),
                    };

                    if let Some(filter) = filter_expr {
                        let filter_val = eval_expr(filter, Some(&row), Some(&schema))?;
                        if !matches!(filter_val, Value::Boolean(true)) {
                            continue;
                        }
                    }

                    let val = if let Some(e) = arg_expr {
                        eval_expr(e, Some(&row), Some(&schema))?
                    } else {
                        Value::Int32(1)
                    };
                    aggs[agg_idx].update(&val)?;
                }
            }

            let mut final_rows = Vec::new();
            let col_names: Vec<String> =
                select.projection.iter().map(get_select_item_name).collect();

            if groups.is_empty() && group_keys_exprs.is_empty() && !agg_funcs.is_empty() {
                let mut default_aggs = Vec::new();
                for (_, agg_expr) in &agg_funcs {
                    match agg_expr {
                        AggExpr::Function(f) => {
                            let name = f.name.0.last().unwrap().value.to_uppercase();
                            if name == "STRING_AGG" {
                                default_aggs.push(Aggregator::new_string_agg(",".to_string()));
                            } else {
                                default_aggs.push(Aggregator::new(&name)?);
                            }
                        }
                        AggExpr::ArrayAgg(_) => {
                            default_aggs.push(Aggregator::new_array_agg());
                        }
                    }
                }
                let mut row_values = Vec::new();
                for (i, _item) in resolved_projection.iter().enumerate() {
                    if let Some(agg_pos) = agg_funcs.iter().position(|(idx, _)| *idx == i) {
                        row_values.push(default_aggs[agg_pos].result());
                    } else {
                        row_values.push(Value::Null);
                    }
                }
                final_rows.push(Row::new(row_values));
            }

            for (key_bytes, aggs) in groups {
                let representative = &group_rows[&key_bytes];

                if let Some(having_expr) = &select.having {
                    let having_val =
                        eval_having_expr(having_expr, representative, &schema, &agg_funcs, &aggs)?;
                    if !matches!(having_val, Value::Boolean(true)) {
                        continue;
                    }
                }

                let mut row_values = Vec::new();

                for (i, item) in resolved_projection.iter().enumerate() {
                    if let Some(agg_pos) = agg_funcs.iter().position(|(idx, _)| *idx == i) {
                        row_values.push(aggs[agg_pos].result());
                    } else {
                        let expr = match item {
                            SelectItem::UnnamedExpr(e)
                            | SelectItem::ExprWithAlias { expr: e, .. } => e,
                            _ => return Err(anyhow!("Unsupported item")),
                        };
                        row_values.push(eval_expr(expr, Some(representative), Some(&schema))?);
                    }
                }
                final_rows.push(Row::new(row_values));
            }

            let result = ExecuteResult::Select {
                column_types: None,
                columns: col_names,
                rows: final_rows,
            };
            if let Some((target_name, _temp)) = select_into_target {
                return self
                    .create_table_from_result(txn, &target_name, result)
                    .await;
            }
            return Ok(result);
        }

        let window_results = if !window_funcs.is_empty() {
            Some(compute_window_functions(
                &filtered_rows,
                &schema,
                &window_funcs,
            )?)
        } else {
            None
        };

        let order_by_references_correlated_subquery = query.order_by.iter().any(|order_expr| {
            if let Expr::Identifier(ref ident) = order_expr.expr {
                for item in &resolved_projection {
                    if let SelectItem::ExprWithAlias { expr, alias } = item {
                        if alias.value.eq_ignore_ascii_case(&ident.value) {
                            if let Expr::Subquery(_) = expr {
                                return true;
                            }
                        }
                    }
                }
            }
            false
        });

        let (filtered_rows, window_results) = if !query.order_by.is_empty()
            && !order_by_references_correlated_subquery
        {
            let mut indexed: Vec<(usize, Row)> = filtered_rows.into_iter().enumerate().collect();
            indexed.sort_by(|(_, a), (_, b)| {
                for order_expr in &query.order_by {
                    let actual_expr = if let Expr::Identifier(ref ident) = order_expr.expr {
                        let mut found_expr = None;
                        for item in &resolved_projection {
                            match item {
                                SelectItem::ExprWithAlias { expr, alias } => {
                                    if alias.value.eq_ignore_ascii_case(&ident.value) {
                                        found_expr = Some(expr);
                                        break;
                                    }
                                }
                                _ => {}
                            }
                        }
                        found_expr.unwrap_or(&order_expr.expr)
                    } else {
                        &order_expr.expr
                    };

                    let val_a =
                        eval_expr(actual_expr, Some(a), Some(&schema)).unwrap_or(Value::Null);
                    let val_b =
                        eval_expr(actual_expr, Some(b), Some(&schema)).unwrap_or(Value::Null);
                    let cmp = super::expr::compare_values(&val_a, &val_b).unwrap_or(0);
                    if cmp != 0 {
                        let asc = order_expr.asc.unwrap_or(true);
                        return if asc {
                            if cmp > 0 {
                                std::cmp::Ordering::Greater
                            } else {
                                std::cmp::Ordering::Less
                            }
                        } else {
                            if cmp > 0 {
                                std::cmp::Ordering::Less
                            } else {
                                std::cmp::Ordering::Greater
                            }
                        };
                    }
                }
                std::cmp::Ordering::Equal
            });
            let reordered_wr = window_results.map(|wr| {
                indexed
                    .iter()
                    .map(|(orig_idx, _)| wr[*orig_idx].clone())
                    .collect()
            });
            let reordered_rows: Vec<Row> = indexed.into_iter().map(|(_, r)| r).collect();
            (reordered_rows, reordered_wr)
        } else {
            (filtered_rows, window_results)
        };

        let mut final_rows = filtered_rows;
        if let Some(offset) = &query.offset {
            if let Ok(v) = eval_expr(&offset.value, None, None) {
                let n = match v {
                    Value::Int64(n) => n as usize,
                    Value::Int32(n) => n as usize,
                    _ => 0,
                };
                final_rows = final_rows.into_iter().skip(n).collect();
            }
        }
        if let Some(limit) = &query.limit {
            if let Ok(v) = eval_expr(limit, None, None) {
                let n = match v {
                    Value::Int64(n) => n as usize,
                    Value::Int32(n) => n as usize,
                    _ => usize::MAX,
                };
                final_rows = final_rows.into_iter().take(n).collect();
            }
        }

        // FETCH FIRST N ROWS ONLY (SQL standard, equivalent to LIMIT)
        if let Some(fetch) = &query.fetch {
            if let Some(quantity) = &fetch.quantity {
                if let Ok(v) = eval_expr(quantity, None, None) {
                    let n = match v {
                        Value::Int64(n) => n as usize,
                        Value::Int32(n) => n as usize,
                        _ => 1,
                    };
                    final_rows = final_rows.into_iter().take(n).collect();
                }
            } else {
                // FETCH FIRST ROW ONLY (no quantity means 1 row)
                final_rows = final_rows.into_iter().take(1).collect();
            }
        }

        let has_window_funcs = !window_funcs.is_empty();
        let wildcard = select
            .projection
            .iter()
            .any(|p| matches!(p, SelectItem::Wildcard(_)));
        // Only use fast-path if projection is exactly `*` with no other items
        let pure_wildcard = wildcard && select.projection.len() == 1;

        // Apply DISTINCT ON before projection (rows still have all original columns)
        let rows_for_projection = match &select.distinct {
            Some(Distinct::On(on_exprs)) => distinct_on_rows(final_rows, on_exprs, Some(&schema)),
            _ => final_rows,
        };

        let mut cols = Vec::new();
        let mut result_rows = Vec::new();

        if pure_wildcard && !has_window_funcs {
            for c in &schema.columns {
                cols.push(c.name.clone());
            }
            result_rows = rows_for_projection;
        } else {
            for item in &select.projection {
                match item {
                    SelectItem::Wildcard(_) => {
                        for c in &schema.columns {
                            cols.push(c.name.clone());
                        }
                    }
                    _ => {
                        let col_name = get_select_item_name(item);
                        tracing::info!("Column name for SELECT item: '{}'", col_name);
                        cols.push(col_name);
                    }
                }
            }

            for (row_idx, row) in rows_for_projection.iter().enumerate() {
                let mut row_values = Vec::new();
                for (proj_idx, item) in resolved_projection.iter().enumerate() {
                    if let Some(wf_pos) = window_funcs.iter().position(|wf| wf.proj_idx == proj_idx)
                    {
                        if let Some(ref wr) = window_results {
                            row_values.push(wr[row_idx][wf_pos].clone());
                        } else {
                            row_values.push(Value::Null);
                        }
                    } else {
                        let expr = match item {
                            SelectItem::UnnamedExpr(e) => e,
                            SelectItem::ExprWithAlias { expr: e, .. } => e,
                            SelectItem::Wildcard(_) => {
                                row_values.extend(row.values.clone());
                                continue;
                            }
                            _ => return Err(anyhow!("Unsupported select item")),
                        };
                        let value = if let Expr::Subquery(subquery) = expr {
                            self.eval_correlated_subquery(txn, subquery, &outer_alias, &schema, row)
                                .await?
                        } else {
                            eval_expr(expr, Some(row), Some(&schema))?
                        };
                        row_values.push(value);
                    }
                }
                result_rows.push(Row::new(row_values));
            }
        }

        if order_by_references_correlated_subquery && !query.order_by.is_empty() {
            result_rows.sort_by(|a, b| {
                for order_expr in &query.order_by {
                    let col_idx = if let Expr::Identifier(ref ident) = order_expr.expr {
                        cols.iter()
                            .position(|c| c.eq_ignore_ascii_case(&ident.value))
                    } else if let Expr::Value(SqlValue::Number(n, _)) = &order_expr.expr {
                        n.parse::<usize>().ok().map(|i| i.saturating_sub(1))
                    } else {
                        None
                    };

                    if let Some(idx) = col_idx {
                        let val_a = a.values.get(idx).cloned().unwrap_or(Value::Null);
                        let val_b = b.values.get(idx).cloned().unwrap_or(Value::Null);
                        let cmp = super::expr::compare_values(&val_a, &val_b).unwrap_or(0);
                        if cmp != 0 {
                            let asc = order_expr.asc.unwrap_or(true);
                            return if asc {
                                if cmp > 0 {
                                    std::cmp::Ordering::Greater
                                } else {
                                    std::cmp::Ordering::Less
                                }
                            } else if cmp > 0 {
                                std::cmp::Ordering::Less
                            } else {
                                std::cmp::Ordering::Greater
                            };
                        }
                    }
                }
                std::cmp::Ordering::Equal
            });
        }

        if matches!(&select.distinct, Some(Distinct::Distinct)) {
            result_rows = dedup_rows(result_rows);
        }

        let column_types = Some(
            select
                .projection
                .iter()
                .flat_map(|item| match item {
                    SelectItem::Wildcard(_) => schema
                        .columns
                        .iter()
                        .map(|c| c.data_type.clone())
                        .collect::<Vec<_>>(),
                    SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                        vec![infer_expr_type(expr, &schema)]
                    }
                    _ => vec![DataType::Text],
                })
                .collect(),
        );

        let result = ExecuteResult::Select {
            column_types,
            columns: cols,
            rows: result_rows,
        };
        if let Some((target_name, _temp)) = select_into_target {
            return self
                .create_table_from_result(txn, &target_name, result)
                .await;
        }
        Ok(result)
    }

    async fn execute_tableless_query(
        &self,
        txn: &mut Transaction,
        select: &sqlparser::ast::Select,
    ) -> Result<ExecuteResult> {
        let resolved_projection = self
            .resolve_projection_subqueries(txn, &select.projection)
            .await?;

        let mut cols = Vec::new();
        let mut values = Vec::new();

        for item in &resolved_projection {
            match item {
                SelectItem::UnnamedExpr(expr) => {
                    cols.push("?column?".to_string());
                    values.push(eval_expr(expr, None, None)?);
                }
                SelectItem::ExprWithAlias { expr, alias } => {
                    cols.push(alias.value.clone());
                    values.push(eval_expr(expr, None, None)?);
                }
                _ => return Err(anyhow!("Unsupported select item in tableless query")),
            }
        }

        Ok(ExecuteResult::Select {
            column_types: None,
            columns: cols,
            rows: vec![Row::new(values)],
        })
    }

    fn execute_set_operation<'a>(
        &'a self,
        txn: &'a mut Transaction,
        op: &'a SetOperator,
        quantifier: &'a SetQuantifier,
        left: &'a SetExpr,
        right: &'a SetExpr,
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ExecuteResult>> + Send + 'a>>
    {
        Box::pin(async move {
            let left_result = self.execute_set_expr(txn, left, ctes).await?;
            let right_result = self.execute_set_expr(txn, right, ctes).await?;

            let (left_cols, left_rows) = match left_result {
                ExecuteResult::Select {
                    columns,
                    column_types: _,
                    rows,
                } => (columns, rows),
                _ => return Err(anyhow!("Left side of set operation must be SELECT")),
            };
            let (right_cols, right_rows) = match right_result {
                ExecuteResult::Select {
                    columns,
                    column_types: _,
                    rows,
                } => (columns, rows),
                _ => return Err(anyhow!("Right side of set operation must be SELECT")),
            };

            if left_cols.len() != right_cols.len() {
                return Err(anyhow!("Column count mismatch in set operation"));
            }

            let is_all = query::is_set_quantifier_all(quantifier);
            let rows = match op {
                SetOperator::Union => query::apply_union(left_rows, right_rows, is_all),
                SetOperator::Intersect => query::apply_intersect(left_rows, right_rows, is_all),
                SetOperator::Except => query::apply_except(left_rows, right_rows, is_all),
            };

            Ok(ExecuteResult::Select {
                column_types: None,
                columns: left_cols,
                rows,
            })
        })
    }

    fn execute_set_expr<'a>(
        &'a self,
        txn: &'a mut Transaction,
        expr: &'a SetExpr,
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ExecuteResult>> + Send + 'a>>
    {
        Box::pin(async move {
            match expr {
                SetExpr::Select(s) => {
                    let query = Query {
                        with: None,
                        body: Box::new(SetExpr::Select(s.clone())),
                        order_by: vec![],
                        limit: None,
                        offset: None,
                        fetch: None,
                        locks: vec![],
                        limit_by: vec![],
                        for_clause: None,
                    };
                    self.execute_query_with_ctes(txn, &query, ctes).await
                }
                SetExpr::SetOperation {
                    op,
                    set_quantifier,
                    left,
                    right,
                } => {
                    self.execute_set_operation(txn, op, set_quantifier, left, right, ctes)
                        .await
                }
                _ => Err(anyhow!("Unsupported set expression")),
            }
        })
    }
    async fn execute_show_tables(&self, txn: &mut Transaction) -> Result<ExecuteResult> {
        let tables = self.store.list_tables(txn).await?;
        Ok(ExecuteResult::ShowTables { tables })
    }

    async fn execute_explain(
        &self,
        txn: &mut Transaction,
        statement: &Statement,
        _analyze: bool,
        _verbose: bool,
    ) -> Result<ExecuteResult> {
        let tables = self.store.list_tables(txn).await?;
        let mut schemas: HashMap<String, TableSchema> = HashMap::new();
        for table_name in &tables {
            if let Ok(Some(schema)) = self.store.get_schema(txn, table_name).await {
                schemas.insert(table_name.clone(), schema);
            }
        }

        let schema_lookup =
            |table_name: &str| -> Option<TableSchema> { schemas.get(table_name).cloned() };

        let row_count_lookup = |_table_name: &str| -> usize { 1000 };

        let plan = explain::generate_plan(statement, schema_lookup, row_count_lookup);
        let plan_text = explain::format_plan_text(&plan, 0);

        let lines: Vec<Row> = plan_text
            .lines()
            .map(|line| Row::new(vec![Value::Text(line.to_string())]))
            .collect();

        Ok(ExecuteResult::Select {
            column_types: None,
            columns: vec!["QUERY PLAN".to_string()],
            rows: lines,
        })
    }

    pub(crate) fn resolve_subqueries<'a>(
        &'a self,
        txn: &'a mut Transaction,
        expr: &'a Expr,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Expr>> + Send + 'a>> {
        Box::pin(async move {
            match expr {
                Expr::InSubquery {
                    expr: inner_expr,
                    subquery,
                    negated,
                } => {
                    let result = self.execute_query(txn, subquery).await?;
                    let values = match result {
                        ExecuteResult::Select { rows, .. } => rows
                            .iter()
                            .filter_map(|row| row.values.first().cloned())
                            .map(|v| value_to_sql_expr(&v))
                            .collect::<Vec<_>>(),
                        _ => return Err(anyhow!("Subquery must return a SELECT result")),
                    };
                    let resolved_inner = Box::new(self.resolve_subqueries(txn, inner_expr).await?);
                    Ok(Expr::InList {
                        expr: resolved_inner,
                        list: values,
                        negated: *negated,
                    })
                }
                Expr::BinaryOp { left, op, right } => {
                    let resolved_left = Box::new(self.resolve_subqueries(txn, left).await?);
                    let resolved_right = Box::new(self.resolve_subqueries(txn, right).await?);
                    Ok(Expr::BinaryOp {
                        left: resolved_left,
                        op: op.clone(),
                        right: resolved_right,
                    })
                }
                Expr::UnaryOp { op, expr: inner } => {
                    let resolved = Box::new(self.resolve_subqueries(txn, inner).await?);
                    Ok(Expr::UnaryOp {
                        op: op.clone(),
                        expr: resolved,
                    })
                }
                Expr::Nested(inner) => {
                    let resolved = Box::new(self.resolve_subqueries(txn, inner).await?);
                    Ok(Expr::Nested(resolved))
                }
                Expr::Subquery(subquery) => {
                    let result = self.execute_query(txn, subquery).await?;
                    match result {
                        ExecuteResult::Select { rows, .. } => {
                            if rows.is_empty() {
                                Ok(Expr::Value(SqlValue::Null))
                            } else if rows.len() == 1 {
                                let value = rows[0].values.first().cloned().unwrap_or(Value::Null);
                                Ok(value_to_sql_expr(&value))
                            } else {
                                Err(anyhow!("Scalar subquery returned more than one row"))
                            }
                        }
                        _ => Err(anyhow!("Subquery must return a SELECT result")),
                    }
                }
                Expr::Exists { subquery, negated } => {
                    let result = self.execute_query(txn, subquery).await?;
                    let exists = match result {
                        ExecuteResult::Select { rows, .. } => !rows.is_empty(),
                        _ => false,
                    };
                    let result_bool = if *negated { !exists } else { exists };
                    Ok(Expr::Value(SqlValue::Boolean(result_bool)))
                }
                Expr::Case {
                    operand,
                    conditions,
                    results,
                    else_result,
                } => {
                    let resolved_operand = if let Some(op) = operand {
                        Some(Box::new(self.resolve_subqueries(txn, op).await?))
                    } else {
                        None
                    };
                    let mut resolved_conditions = Vec::with_capacity(conditions.len());
                    for cond in conditions {
                        resolved_conditions.push(self.resolve_subqueries(txn, cond).await?);
                    }
                    let mut resolved_results = Vec::with_capacity(results.len());
                    for res in results {
                        resolved_results.push(self.resolve_subqueries(txn, res).await?);
                    }
                    let resolved_else = if let Some(else_expr) = else_result {
                        Some(Box::new(self.resolve_subqueries(txn, else_expr).await?))
                    } else {
                        None
                    };
                    Ok(Expr::Case {
                        operand: resolved_operand,
                        conditions: resolved_conditions,
                        results: resolved_results,
                        else_result: resolved_else,
                    })
                }
                Expr::Function(func) => {
                    let mut resolved_args = Vec::new();
                    for arg in &func.args {
                        let resolved_arg = match arg {
                            sqlparser::ast::FunctionArg::Unnamed(
                                sqlparser::ast::FunctionArgExpr::Expr(e),
                            ) => sqlparser::ast::FunctionArg::Unnamed(
                                sqlparser::ast::FunctionArgExpr::Expr(
                                    self.resolve_subqueries(txn, e).await?,
                                ),
                            ),
                            other => other.clone(),
                        };
                        resolved_args.push(resolved_arg);
                    }
                    Ok(Expr::Function(sqlparser::ast::Function {
                        name: func.name.clone(),
                        args: resolved_args,
                        filter: func.filter.clone(),
                        null_treatment: func.null_treatment.clone(),
                        over: func.over.clone(),
                        distinct: func.distinct,
                        special: func.special,
                        order_by: func.order_by.clone(),
                    }))
                }
                _ => Ok(expr.clone()),
            }
        })
    }

    pub(crate) async fn resolve_projection_subqueries(
        &self,
        txn: &mut Transaction,
        projection: &[SelectItem],
    ) -> Result<Vec<SelectItem>> {
        let mut resolved = Vec::with_capacity(projection.len());
        for item in projection {
            let resolved_item = match item {
                SelectItem::UnnamedExpr(e) => {
                    SelectItem::UnnamedExpr(self.resolve_subqueries(txn, e).await?)
                }
                SelectItem::ExprWithAlias { expr, alias } => SelectItem::ExprWithAlias {
                    expr: self.resolve_subqueries(txn, expr).await?,
                    alias: alias.clone(),
                },
                other => other.clone(),
            };
            resolved.push(resolved_item);
        }
        Ok(resolved)
    }

    #[allow(dead_code)]
    pub(crate) async fn resolve_projection_subqueries_with_outer_context(
        &self,
        txn: &mut Transaction,
        projection: &[SelectItem],
        outer_alias: &str,
    ) -> Result<Vec<SelectItem>> {
        let mut resolved = Vec::with_capacity(projection.len());
        for item in projection {
            let resolved_item = match item {
                SelectItem::UnnamedExpr(e) => {
                    if self.expr_is_correlated_subquery(e, outer_alias) {
                        SelectItem::UnnamedExpr(e.clone())
                    } else {
                        SelectItem::UnnamedExpr(self.resolve_subqueries(txn, e).await?)
                    }
                }
                SelectItem::ExprWithAlias { expr, alias } => {
                    if self.expr_is_correlated_subquery(expr, outer_alias) {
                        SelectItem::ExprWithAlias {
                            expr: expr.clone(),
                            alias: alias.clone(),
                        }
                    } else {
                        SelectItem::ExprWithAlias {
                            expr: self.resolve_subqueries(txn, expr).await?,
                            alias: alias.clone(),
                        }
                    }
                }
                other => other.clone(),
            };
            resolved.push(resolved_item);
        }
        Ok(resolved)
    }

    fn expr_is_correlated_subquery(&self, expr: &Expr, outer_alias: &str) -> bool {
        match expr {
            Expr::Subquery(q) => query_has_outer_reference(q, outer_alias),
            _ => false,
        }
    }

    fn expr_has_correlated_exists(&self, expr: &Expr, outer_alias: &str) -> bool {
        match expr {
            Expr::Exists { subquery, .. } => query_has_outer_reference(subquery, outer_alias),
            Expr::BinaryOp { left, right, .. } => {
                self.expr_has_correlated_exists(left, outer_alias)
                    || self.expr_has_correlated_exists(right, outer_alias)
            }
            Expr::UnaryOp { expr: inner, .. } => {
                self.expr_has_correlated_exists(inner, outer_alias)
            }
            Expr::Nested(inner) => self.expr_has_correlated_exists(inner, outer_alias),
            _ => false,
        }
    }

    fn eval_correlated_exists<'a>(
        &'a self,
        txn: &'a mut Transaction,
        subquery: &'a Query,
        negated: bool,
        outer_alias: &'a str,
        outer_schema: &'a TableSchema,
        outer_row: &'a Row,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value>> + Send + 'a>> {
        Box::pin(async move {
            let substituted_query =
                substitute_outer_values_in_query(subquery, outer_alias, outer_schema, outer_row);
            let result = self.execute_query(txn, &substituted_query).await?;
            let exists = match result {
                ExecuteResult::Select { rows, .. } => !rows.is_empty(),
                _ => false,
            };
            let result_bool = if negated { !exists } else { exists };
            Ok(Value::Boolean(result_bool))
        })
    }

    fn eval_selection_with_correlated_exists<'a>(
        &'a self,
        txn: &'a mut Transaction,
        expr: &'a Expr,
        outer_alias: &'a str,
        outer_schema: &'a TableSchema,
        outer_row: &'a Row,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value>> + Send + 'a>> {
        Box::pin(async move {
            match expr {
                Expr::Exists { subquery, negated } => {
                    if query_has_outer_reference(subquery, outer_alias) {
                        self.eval_correlated_exists(
                            txn,
                            subquery,
                            *negated,
                            outer_alias,
                            outer_schema,
                            outer_row,
                        )
                        .await
                    } else {
                        let result = self.execute_query(txn, subquery).await?;
                        let exists = match result {
                            ExecuteResult::Select { rows, .. } => !rows.is_empty(),
                            _ => false,
                        };
                        let result_bool = if *negated { !exists } else { exists };
                        Ok(Value::Boolean(result_bool))
                    }
                }
                Expr::BinaryOp { left, op, right } => {
                    let left_val = self
                        .eval_selection_with_correlated_exists(
                            txn,
                            left,
                            outer_alias,
                            outer_schema,
                            outer_row,
                        )
                        .await?;
                    let right_val = self
                        .eval_selection_with_correlated_exists(
                            txn,
                            right,
                            outer_alias,
                            outer_schema,
                            outer_row,
                        )
                        .await?;
                    match op {
                        BinaryOperator::And => {
                            let left_bool = matches!(left_val, Value::Boolean(true));
                            let right_bool = matches!(right_val, Value::Boolean(true));
                            Ok(Value::Boolean(left_bool && right_bool))
                        }
                        BinaryOperator::Or => {
                            let left_bool = matches!(left_val, Value::Boolean(true));
                            let right_bool = matches!(right_val, Value::Boolean(true));
                            Ok(Value::Boolean(left_bool || right_bool))
                        }
                        _ => eval_expr(expr, Some(outer_row), Some(outer_schema)),
                    }
                }
                Expr::UnaryOp {
                    op: sqlparser::ast::UnaryOperator::Not,
                    expr: inner,
                } => {
                    let inner_val = self
                        .eval_selection_with_correlated_exists(
                            txn,
                            inner,
                            outer_alias,
                            outer_schema,
                            outer_row,
                        )
                        .await?;
                    let inner_bool = matches!(inner_val, Value::Boolean(true));
                    Ok(Value::Boolean(!inner_bool))
                }
                Expr::Nested(inner) => {
                    self.eval_selection_with_correlated_exists(
                        txn,
                        inner,
                        outer_alias,
                        outer_schema,
                        outer_row,
                    )
                    .await
                }
                _ => eval_expr(expr, Some(outer_row), Some(outer_schema)),
            }
        })
    }

    fn eval_correlated_subquery<'a>(
        &'a self,
        txn: &'a mut Transaction,
        subquery: &'a Query,
        outer_alias: &'a str,
        outer_schema: &'a TableSchema,
        outer_row: &'a Row,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value>> + Send + 'a>> {
        Box::pin(async move {
            let substituted_query =
                substitute_outer_values_in_query(subquery, outer_alias, outer_schema, outer_row);
            let result = self.execute_query(txn, &substituted_query).await?;
            match result {
                ExecuteResult::Select { rows, .. } => {
                    if rows.is_empty() {
                        Ok(Value::Null)
                    } else if rows.len() == 1 {
                        Ok(rows[0].values.first().cloned().unwrap_or(Value::Null))
                    } else {
                        Err(anyhow!("Scalar subquery returned more than one row"))
                    }
                }
                _ => Err(anyhow!("Subquery must return a SELECT result")),
            }
        })
    }

    pub(crate) async fn scan_and_fill(
        &self,
        txn: &mut Transaction,
        table_name: &str,
        schema: &TableSchema,
    ) -> Result<Vec<Row>> {
        let rows = self.store.scan(txn, table_name).await?;
        let mut filled_rows = Vec::with_capacity(rows.len());
        for mut row in rows {
            fill_row_defaults(&mut row, schema)?;
            filled_rows.push(row);
        }
        Ok(filled_rows)
    }

    pub fn parse_value_for_copy(&self, val: &str, data_type: &DataType) -> Value {
        parse_value_for_copy(val, data_type)
    }

    pub async fn execute_copy_insert(
        &self,
        session: &mut Session,
        table_name: &str,
        col_values: Vec<(String, Value)>,
    ) -> Result<()> {
        let is_autocommit = !session.is_in_transaction();

        if is_autocommit {
            session.begin().await?;
        }

        let result = async {
            let txn = session.get_mut_txn().expect("Transaction must be active");
            let schema = self
                .store
                .get_schema(txn, table_name)
                .await?
                .ok_or_else(|| anyhow!("Table '{}' not found", table_name))?;

            let mut row_values = vec![Value::Null; schema.columns.len()];

            for (col_name, value) in col_values {
                if let Some(idx) = schema.column_index(&col_name) {
                    row_values[idx] = value;
                }
            }

            for (i, col) in schema.columns.iter().enumerate() {
                if matches!(row_values[i], Value::Null) {
                    if col.is_serial {
                        let next_id = self.store.next_sequence_value(txn, schema.table_id).await?;
                        row_values[i] = Value::Int32(next_id);
                    } else if let Some(ref default_expr) = col.default_expr {
                        row_values[i] = eval_default_expr(default_expr)?;
                    }
                }
            }

            let mut row = Row { values: row_values };
            fill_row_defaults(&mut row, &schema)?;

            self.store.insert(txn, &schema.name, row.clone()).await?;

            let pk_values = schema.get_pk_values(&row);
            for index in &schema.indexes {
                let idx_values = schema.get_index_values(index, &row);
                self.store
                    .create_index_entry(
                        txn,
                        schema.table_id,
                        index.id,
                        &idx_values,
                        &pk_values,
                        index.unique,
                    )
                    .await?;
            }

            Ok::<(), anyhow::Error>(())
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
