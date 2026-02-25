//! DROP TABLE, TRUNCATE TABLE, and DROP INDEX operations.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use sqlparser::ast::ObjectName;
use tikv_client::Transaction;

use crate::model::{DataType, Row, TableSchema};
use crate::sql::gin::extract_gin_token_hashes_from_row;
use crate::sql::index_helpers;
use crate::sql::names;
use crate::sql::projection::fill_row_defaults;
use crate::sql::ExecuteResult;
use crate::storage::TikvStore;

use super::{
    drop_dependent_views, drop_owned_sequences_for_table, KvScanBatches, DDL_SCAN_BATCH_SIZE,
};

pub async fn execute_drop_table(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    names: &[ObjectName],
    if_exists: bool,
    cascade: bool,
    stats_cache: &crate::sql::stats::TableStatsCache,
) -> Result<ExecuteResult> {
    let mut last = String::new();
    for name in names {
        let resolved =
            match names::resolve_existing_table_name(store.as_ref(), txn, db_id, name, search_path)
                .await?
            {
                Some(resolved) => resolved,
                None => {
                    if !if_exists {
                        return Err(anyhow!("Table '{}' does not exist", name));
                    }
                    continue;
                }
            };

        // Resolve table_id for cache invalidation before the schema is deleted.
        let table_id = store
            .get_schema(txn, db_id, &resolved.full)
            .await?
            .map(|s| s.table_id);

        // CASCADE: drop views that depend on this table.
        if cascade {
            let _dropped = drop_dependent_views(store, txn, db_id, &resolved.full).await?;
        }

        for trigger in store
            .list_triggers_for_table(txn, db_id, &resolved.full)
            .await?
        {
            let _ = store
                .drop_trigger(txn, db_id, &resolved.full, &trigger.name)
                .await?;
        }
        drop_owned_sequences_for_table(store, txn, db_id, &resolved.full).await?;
        store.drop_table(txn, db_id, &resolved.full).await?;

        // Invalidate the in-memory stats cache (persistent stats deleted by drop_table).
        if let Some(tid) = table_id {
            stats_cache.invalidate(db_id, tid);
        }

        last = resolved.full;
    }
    Ok(ExecuteResult::DropTable { table_name: last })
}

pub async fn execute_truncate(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    table_name: &ObjectName,
) -> Result<ExecuteResult> {
    let resolved =
        names::resolve_existing_table_name(store.as_ref(), txn, db_id, table_name, search_path)
            .await?
            .ok_or_else(|| anyhow!("Table '{}' does not exist", table_name))?;
    let t = resolved.full;
    if !store.truncate_table(txn, db_id, &t).await? {
        return Err(anyhow!("Table '{}' does not exist", t));
    }
    Ok(ExecuteResult::TruncateTable { table_name: t })
}

pub async fn execute_drop_index(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    idx_name: &str,
    schema: &mut TableSchema,
    _table_name: &str,
    rows: Vec<Row>,
) -> Result<Option<String>> {
    if let Some(pos) = schema.indexes.iter().position(|i| i.name == idx_name) {
        let index = schema.indexes.remove(pos);
        if schema.pk_indices.is_empty() {
            let pk_types: Vec<DataType> = vec![DataType::Uuid];
            let (start, end) = crate::storage::encode_table_data_range_v2(db_id, schema.table_id);
            let data_key_prefix = start.clone();
            let mut scanner = KvScanBatches::new(start, end, DDL_SCAN_BATCH_SIZE);
            while let Some(batch) = scanner.next_batch(txn).await? {
                for pair in batch {
                    let key: &[u8] = pair.key().as_ref().into();
                    let pk_bytes =
                        key.strip_prefix(data_key_prefix.as_slice())
                            .ok_or_else(|| {
                                anyhow!("corrupted row key while dropping index '{}'", idx_name)
                            })?;
                    let pk_values =
                        crate::storage::decode_pk_from_index_suffix(pk_bytes, &pk_types)?;

                    let mut row = crate::storage::deserialize_row(pair.value())?;
                    fill_row_defaults(&mut row, schema)?;

                    let gin_hashes = extract_gin_token_hashes_from_row(schema, &index, &row)?;
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

                    if !index_helpers::is_index_materializable(&index) {
                        continue;
                    }

                    if !index_helpers::eval_index_predicate(&index, schema, &row)? {
                        continue;
                    }
                    let idx_values =
                        index_helpers::get_index_values_with_expressions(&index, schema, &row)?;
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
            }
        } else {
            for row in rows {
                let pk_values = schema.get_pk_values(&row);

                let gin_hashes = extract_gin_token_hashes_from_row(schema, &index, &row)?;
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

                if !index_helpers::is_index_materializable(&index) {
                    continue;
                }

                if !index_helpers::eval_index_predicate(&index, schema, &row)? {
                    continue;
                }
                let idx_values =
                    index_helpers::get_index_values_with_expressions(&index, schema, &row)?;
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
        }
        store.update_schema(txn, db_id, schema.clone()).await?;

        // Release the reservation key for the dropped index name.
        let owning_schema = _table_name.splitn(2, '.').next().unwrap_or("public");
        let idx_full = format!("{}.{}", owning_schema, idx_name);
        store.release_relation_name(txn, db_id, &idx_full).await?;

        return Ok(Some(idx_name.to_string()));
    }
    Ok(None)
}
