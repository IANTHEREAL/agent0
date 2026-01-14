//! DDL operation execution (CREATE TABLE AS, CREATE/DROP INDEX, ALTER TABLE)

use super::ddl;
use super::ExecuteResult;
use super::Executor;
use anyhow::{anyhow, Result};
use sqlparser::ast::{
    AlterTableOperation, ColumnDef as SqlColumnDef, ObjectName, OrderByExpr, Query,
};
use tikv_client::Transaction;

impl Executor {
    pub(crate) async fn execute_create_table_as(
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
        idx_name: &str,
        table_name: &ObjectName,
        columns: &[OrderByExpr],
        unique: bool,
        if_not_exists: bool,
    ) -> Result<ExecuteResult> {
        let tbl_name = table_name.0.last().unwrap().value.clone();
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
            table_name,
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
        name: &ObjectName,
        operation: &AlterTableOperation,
    ) -> Result<ExecuteResult> {
        let t = name.0.last().unwrap().value.clone();
        let schema = self
            .store()
            .get_schema(txn, &t)
            .await?
            .ok_or_else(|| anyhow!("Table '{}' does not exist", t))?;
        let rows = self.scan_and_fill(txn, &t, &schema).await?;
        ddl::execute_alter_table(&self.store(), txn, name, operation, rows).await
    }
}
