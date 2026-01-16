//! DDL operation execution (CREATE TABLE AS, CREATE/DROP INDEX, ALTER TABLE)

use super::ddl;
use super::names;
use super::ExecuteResult;
use super::Executor;
use anyhow::{anyhow, Result};
use sqlparser::ast::{
    AlterTableOperation, ColumnDef as SqlColumnDef, ObjectName, OrderByExpr, Query,
};
use std::collections::HashMap;
use tikv_client::Transaction;

impl Executor {
    pub(crate) async fn execute_create_table_as(
        &self,
        txn: &mut Transaction,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        name: &ObjectName,
        query: &Query,
        columns: &[SqlColumnDef],
        if_not_exists: bool,
        _temporary: bool,
    ) -> Result<ExecuteResult> {
        let resolved = names::resolve_ddl_object_name(name, search_path)?;
        if !self.store().schema_exists(txn, &resolved.schema).await? {
            return Err(anyhow!("schema '{}' does not exist", resolved.schema));
        }
        let table_name = resolved.full;

        let ctes = self
            .build_cte_context(txn, sequence_values, search_path, query)
            .await?;
        let result = self
            .execute_query_with_ctes(txn, sequence_values, search_path, query, &ctes)
            .await?;

        let (result_cols, result_rows) = match result {
            ExecuteResult::Select {
                columns: cols,
                rows,
                ..
            } => (cols, rows),
            _ => return Err(anyhow!("CREATE TABLE AS requires a SELECT query")),
        };

        ddl::create_table_from_query_result(
            &self.store(),
            txn,
            &table_name,
            if_not_exists,
            result_cols,
            result_rows,
            columns,
        )
        .await
    }

    pub(crate) async fn create_table_from_result(
        &self,
        txn: &mut Transaction,
        search_path: &[String],
        target_name: &ObjectName,
        result: ExecuteResult,
    ) -> Result<ExecuteResult> {
        let resolved = names::resolve_ddl_object_name(target_name, search_path)?;
        if !self.store().schema_exists(txn, &resolved.schema).await? {
            return Err(anyhow!("schema '{}' does not exist", resolved.schema));
        }
        let table_name = resolved.full;

        let (result_cols, result_rows) = match result {
            ExecuteResult::Select {
                columns: cols,
                rows,
                ..
            } => (cols, rows),
            _ => return Err(anyhow!("SELECT INTO requires a SELECT query")),
        };

        ddl::create_table_from_select_into(
            &self.store(),
            txn,
            &table_name,
            result_cols,
            result_rows,
        )
        .await
    }

    pub(crate) async fn execute_create_index(
        &self,
        txn: &mut Transaction,
        search_path: &[String],
        idx_name: &str,
        table_name: &ObjectName,
        columns: &[OrderByExpr],
        unique: bool,
        if_not_exists: bool,
    ) -> Result<ExecuteResult> {
        let resolved =
            names::resolve_existing_table_name(self.store().as_ref(), txn, table_name, search_path)
                .await?
                .ok_or_else(|| anyhow!("Table '{}' does not exist", table_name))?;
        let tbl_name = resolved.full;
        let schema = self
            .store()
            .get_schema(txn, &tbl_name)
            .await?
            .ok_or_else(|| anyhow!("Table not found"))?;
        let rows = self.scan_and_fill(txn, &tbl_name, &schema).await?;
        ddl::execute_create_index(
            &self.store(),
            txn,
            idx_name,
            &tbl_name,
            columns,
            unique,
            if_not_exists,
            rows,
        )
        .await
    }

    pub(crate) async fn execute_drop_index(
        &self,
        txn: &mut Transaction,
        _search_path: &[String],
        names: &[ObjectName],
        if_exists: bool,
    ) -> Result<ExecuteResult> {
        let mut last_index = String::new();
        for name in names {
            let idx_name = name.0.last().unwrap().value.clone();
            let mut found = false;

            let tables = self.store().list_tables(txn).await?;
            for table_name in &tables {
                let mut schema = match self.store().get_schema(txn, table_name).await? {
                    Some(s) => s,
                    None => continue,
                };

                let rows = self.scan_and_fill(txn, table_name, &schema).await?;
                if let Some(dropped) = ddl::execute_drop_index(
                    &self.store(),
                    txn,
                    &idx_name,
                    &mut schema,
                    table_name,
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

    pub(crate) async fn execute_alter_table(
        &self,
        txn: &mut Transaction,
        search_path: &[String],
        name: &ObjectName,
        operation: &AlterTableOperation,
    ) -> Result<ExecuteResult> {
        ddl::execute_alter_table(&self.store(), txn, search_path, name, operation).await
    }
}
