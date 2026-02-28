//! DELETE row execution: storage entry cleanup and index removal.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Result;
use tikv_client::Transaction;

use crate::model::{Row, TableSchema};
use crate::sql::gin::extract_gin_token_hashes_from_row;
use crate::sql::index_helpers;
use crate::storage::TikvStore;
use crate::worker::types::IndexState;

use super::foreign_keys::{handle_foreign_key_on_delete, FkDeleteContext, FkStoreCtx};

pub async fn execute_delete_row(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    table_name: &str,
    schema: &TableSchema,
    row: &Row,
    stmt_deleting_pks: &HashSet<String>,
    fk_ctx: &mut FkDeleteContext,
) -> Result<()> {
    let fk_store_ctx = FkStoreCtx { store, db_id };
    handle_foreign_key_on_delete(
        &fk_store_ctx,
        txn,
        table_name,
        schema,
        row,
        stmt_deleting_pks,
        fk_ctx,
    )
    .await?;

    delete_row_storage_entries(store, txn, db_id, table_name, schema, row).await?;
    fk_ctx.on_statement_row_deleted(table_name, schema, row);
    Ok(())
}

pub(super) async fn delete_row_storage_entries(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    table_name: &str,
    schema: &TableSchema,
    row: &Row,
) -> Result<()> {
    let pk_values = schema.get_pk_values(row);
    store
        .delete_by_pk(txn, db_id, table_name, &pk_values)
        .await?;

    for index in &schema.indexes {
        if matches!(index.state, IndexState::Invalid)
            || (matches!(index.state, IndexState::Building) && !index.unique)
        {
            continue;
        }

        // HNSW uses lazy deletion: the old PK label stays in the graph and
        // is filtered out by the over-fetch strategy in HnswScanOperator.
        if index.is_hnsw() {
            continue;
        }

        let gin_hashes = extract_gin_token_hashes_from_row(schema, index, row)?;
        if !gin_hashes.is_empty() {
            store
                .delete_gin_index_entries(
                    txn,
                    db_id,
                    schema.table_id,
                    index.id,
                    &gin_hashes,
                    &pk_values,
                )
                .await?;
            continue;
        }

        if !index_helpers::is_index_materializable(index) {
            continue;
        }

        if !index_helpers::eval_index_predicate(index, schema, row)? {
            continue;
        }

        let idx_values = index_helpers::get_index_values_with_expressions(index, schema, row)?;
        store
            .delete_index_entry(
                txn,
                db_id,
                schema.table_id,
                index.id,
                &idx_values,
                &pk_values,
                index.unique,
            )
            .await?;
    }

    Ok(())
}
