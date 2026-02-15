use super::catalog::{global_catalog, ScanContext};
use super::query_context::QueryContext;
use crate::storage::TikvStore;
use crate::types::{Row, TableSchema};
use anyhow::{anyhow, Result};
use std::sync::Arc;
use tikv_client::Transaction;

pub fn get_information_schema_schema(table_name: &str) -> Option<TableSchema> {
    let lower = table_name.to_lowercase();
    let name = lower
        .strip_prefix("information_schema.")
        .or_else(|| lower.strip_prefix("pg_catalog."))
        .unwrap_or(&lower);

    global_catalog().get(name).map(|vt| vt.schema())
}

/// Filter hints extracted from WHERE clause to optimize virtual table generation
#[derive(Default, Clone)]
pub struct VirtualTableFilter {
    pub table_name: Option<String>,
    pub table_schema: Option<String>,
}

pub async fn get_information_schema_data_filtered(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    table_name: &str,
    filter: &VirtualTableFilter,
) -> Result<(TableSchema, Vec<Row>)> {
    let lower = table_name.to_lowercase();
    let name = lower
        .strip_prefix("information_schema.")
        .or_else(|| lower.strip_prefix("pg_catalog."))
        .unwrap_or(&lower);

    let vt = global_catalog()
        .get(name)
        .ok_or_else(|| anyhow!("Unknown table"))?;

    let user_tables = if let Some(ref tbl) = filter.table_name {
        let schema_prefix = filter.table_schema.as_deref().unwrap_or("public");
        vec![format!("{}.{}", schema_prefix, tbl)]
    } else {
        store.list_tables(txn, db_id).await?
    };

    let schemas = store.list_schemas(txn, db_id).await?;
    let schema_oids = store.list_schema_oids(txn, db_id).await?;
    let database_name = match QueryContext::current_database_name() {
        Some(name) => name,
        None => Arc::<str>::from(
            store
                .get_database_by_id(txn, db_id)
                .await?
                .ok_or_else(|| anyhow!("database definition not found for db_id={}", db_id))?
                .name,
        ),
    };

    let mut scan_ctx = ScanContext {
        store,
        txn,
        db_id,
        database_name: database_name.as_ref(),
        user_tables: &user_tables,
        schemas: &schemas,
        schema_oids: &schema_oids,
    };
    let rows = vt.scan(&mut scan_ctx).await?;
    Ok((vt.schema(), rows))
}
