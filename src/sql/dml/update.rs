//! UPDATE row execution: index maintenance, PK change detection, and row upsert.

use std::sync::Arc;

use anyhow::Result;
use tikv_client::Transaction;

use crate::model::{DataType, Row, TableSchema, Value};
use crate::sql::error::SqlError;
use crate::sql::gin::extract_gin_token_hashes_from_row;
use crate::sql::hnsw::storage::{
    create_empty_hnsw_index, hnsw_graph_key, hnsw_meta_key, load_hnsw_graph_from_txn,
    serialize_hnsw_snapshot,
};
use crate::sql::hnsw::vec_f64_to_f32;
use crate::sql::hnsw::{hnsw_pk_label, HNSW_DEFAULT_EF_CONSTRUCTION, HNSW_DEFAULT_M};
use crate::sql::index_consistency::{
    is_unique_duplicate_error, resolve_unique_index_conflict, UniqueConflictResolution,
};
use crate::sql::index_helpers;
use crate::storage::TikvStore;
use crate::txn::txn_put;
use crate::worker::types::IndexState;

use super::defaults::coerce_row_values;
use super::foreign_keys::{
    handle_foreign_key_on_update, validate_foreign_keys, FkDeleteContext, FkStoreCtx,
};
use super::insert::validate_enum_values;
use super::EnumLabelCache;

pub async fn execute_update_row_by_pk(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    table_name: &str,
    schema: &TableSchema,
    pk_values: &[Value],
    old_row: &Row,
    new_row: Row,
    enum_cache: &EnumLabelCache,
) -> Result<Row> {
    let mut new_row_values = new_row.values;
    coerce_row_values(schema, &mut new_row_values)?;
    let new_row = Row::new(new_row_values);

    validate_enum_values(schema, &new_row, enum_cache)?;
    if !schema.foreign_keys.is_empty() {
        validate_foreign_keys(store, txn, db_id, schema, &new_row).await?;
    }

    update_row_indexes(store, txn, db_id, schema, pk_values, old_row, &new_row).await?;
    store
        .upsert_by_pk(txn, db_id, table_name, pk_values, new_row.clone())
        .await?;
    Ok(new_row)
}

async fn update_row_indexes(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    schema: &TableSchema,
    pk_values: &[Value],
    old_row: &Row,
    new_row: &Row,
) -> Result<()> {
    for index in &schema.indexes {
        if matches!(index.state, IndexState::Invalid)
            || (matches!(index.state, IndexState::Building) && !index.unique)
        {
            continue;
        }
        if index_helpers::index_values_unchanged(index, schema, old_row, new_row)? {
            continue;
        }

        // HNSW indexes: handled after the loop by maintain_hnsw_indexes_after_update.
        if index.is_hnsw() {
            continue;
        }

        let old_gin_hashes = extract_gin_token_hashes_from_row(schema, index, old_row)?;
        let new_gin_hashes = extract_gin_token_hashes_from_row(schema, index, new_row)?;
        if !old_gin_hashes.is_empty() || !new_gin_hashes.is_empty() {
            if !old_gin_hashes.is_empty() {
                store
                    .delete_gin_index_entries(
                        txn,
                        db_id,
                        schema.table_id,
                        index.id,
                        &old_gin_hashes,
                        pk_values,
                    )
                    .await?;
            }
            if !new_gin_hashes.is_empty() {
                store
                    .create_gin_index_entries(
                        txn,
                        db_id,
                        schema.table_id,
                        index.id,
                        &new_gin_hashes,
                        pk_values,
                    )
                    .await?;
            }
            continue;
        }

        if !index_helpers::is_index_materializable(index) {
            continue;
        }

        let old_matches = index_helpers::eval_index_predicate(index, schema, old_row)?;
        if old_matches {
            let old_idx = index_helpers::get_index_values_with_expressions(index, schema, old_row)?;
            store
                .delete_index_entry(
                    txn,
                    db_id,
                    schema.table_id,
                    index.id,
                    &old_idx,
                    pk_values,
                    index.unique,
                )
                .await?;
        }

        let new_matches = index_helpers::eval_index_predicate(index, schema, new_row)?;
        if new_matches {
            let new_idx = index_helpers::get_index_values_with_expressions(index, schema, new_row)?;
            let create_result = store
                .create_index_entry(
                    txn,
                    db_id,
                    schema.table_id,
                    index.id,
                    &new_idx,
                    pk_values,
                    index.unique,
                )
                .await;
            if let Err(e) = create_result {
                if index.unique
                    && matches!(
                        index.state,
                        IndexState::Building | IndexState::WriteOnly | IndexState::Ready
                    )
                    && is_unique_duplicate_error(&e)
                {
                    match resolve_unique_index_conflict(
                        store, txn, db_id, schema, index, &new_idx, pk_values,
                    )
                    .await?
                    {
                        UniqueConflictResolution::Idempotent
                        | UniqueConflictResolution::StaleReplaced => {}
                        UniqueConflictResolution::RealConflict => return Err(e),
                    }
                } else {
                    return Err(e);
                }
            }
        }
    }

    maintain_hnsw_indexes_after_update(txn, db_id, schema, new_row, pk_values).await?;
    Ok(())
}

pub async fn execute_update_row(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    table_name: &str,
    schema: &TableSchema,
    old_row: &Row,
    new_row: Row,
    enum_cache: &EnumLabelCache,
    fk_ctx: Option<&mut FkDeleteContext>,
) -> Result<Row> {
    execute_update_row_inner(
        store, txn, db_id, table_name, schema, old_row, new_row, enum_cache, fk_ctx, true, true,
    )
    .await
}

pub(crate) async fn execute_update_row_without_fk_update(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    table_name: &str,
    schema: &TableSchema,
    old_row: &Row,
    new_row: Row,
    enum_cache: &EnumLabelCache,
    fk_ctx: Option<&mut FkDeleteContext>,
) -> Result<Row> {
    execute_update_row_inner(
        store, txn, db_id, table_name, schema, old_row, new_row, enum_cache, fk_ctx, false, false,
    )
    .await
}

async fn execute_update_row_inner(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    table_name: &str,
    schema: &TableSchema,
    old_row: &Row,
    new_row: Row,
    enum_cache: &EnumLabelCache,
    fk_ctx: Option<&mut FkDeleteContext>,
    propagate_fk_update: bool,
    validate_fk_now: bool,
) -> Result<Row> {
    let mut new_row_values = new_row.values;
    coerce_row_values(schema, &mut new_row_values)?;
    let new_row = Row::new(new_row_values);

    validate_enum_values(schema, &new_row, enum_cache)?;

    let old_pks = schema.get_pk_values(old_row);
    let new_pks = schema.get_pk_values(&new_row);
    let pk_changed = old_pks != new_pks;

    if validate_fk_now && !schema.foreign_keys.is_empty() {
        validate_foreign_keys(store, txn, db_id, schema, &new_row).await?;
    }

    if pk_changed {
        let existing = store
            .batch_get_rows(txn, db_id, schema.table_id, vec![new_pks.clone()], schema)
            .await?;
        if !existing.is_empty() {
            let pk_cols: Vec<_> = schema
                .pk_indices
                .iter()
                .map(|&i| schema.columns[i].name.clone())
                .collect();
            let pk_vals: Vec<_> = new_pks.iter().map(|v| format!("{}", v)).collect();
            let default_pk_name = format!(
                "{}_pkey",
                schema.name.rsplit('.').next().unwrap_or(&schema.name)
            );
            let pk_constraint_name = schema.pk_constraint_name.clone().unwrap_or(default_pk_name);
            return Err(SqlError::UniqueViolation {
                constraint: pk_constraint_name.clone(),
                message: format!(
                    "duplicate key value violates unique constraint \"{}\"\nDETAIL:  Key ({})=({}) already exists.",
                    pk_constraint_name, pk_cols.join(", "), pk_vals.join(", ")
                ),
                row_offset: None,
            }.into());
        }
    }

    for index in &schema.indexes {
        if matches!(index.state, IndexState::Invalid)
            || (matches!(index.state, IndexState::Building) && !index.unique)
        {
            continue;
        }
        if !pk_changed && index_helpers::index_values_unchanged(index, schema, old_row, &new_row)? {
            continue;
        }

        // HNSW indexes: handled after the create-side loop by
        // maintain_hnsw_indexes_after_update.
        if index.is_hnsw() {
            continue;
        }

        let gin_hashes = extract_gin_token_hashes_from_row(schema, index, old_row)?;
        if !gin_hashes.is_empty() {
            store
                .delete_gin_index_entries(
                    txn,
                    db_id,
                    schema.table_id,
                    index.id,
                    &gin_hashes,
                    &old_pks,
                )
                .await?;
            continue;
        }

        if !index_helpers::is_index_materializable(index) {
            continue;
        }
        let old_matches = index_helpers::eval_index_predicate(index, schema, old_row)?;
        if old_matches {
            let old_idx = index_helpers::get_index_values_with_expressions(index, schema, old_row)?;
            store
                .delete_index_entry(
                    txn,
                    db_id,
                    schema.table_id,
                    index.id,
                    &old_idx,
                    &old_pks,
                    index.unique,
                )
                .await?;
        }
    }

    if pk_changed {
        store.delete_by_pk(txn, db_id, table_name, &old_pks).await?;
    }

    store
        .upsert(txn, db_id, table_name, new_row.clone())
        .await?;

    for index in &schema.indexes {
        if matches!(index.state, IndexState::Invalid)
            || (matches!(index.state, IndexState::Building) && !index.unique)
        {
            continue;
        }
        if !pk_changed && index_helpers::index_values_unchanged(index, schema, old_row, &new_row)? {
            continue;
        }

        // HNSW: handled after this loop by maintain_hnsw_indexes_after_update.
        if index.is_hnsw() {
            continue;
        }

        let gin_hashes = extract_gin_token_hashes_from_row(schema, index, &new_row)?;
        if !gin_hashes.is_empty() {
            store
                .create_gin_index_entries(
                    txn,
                    db_id,
                    schema.table_id,
                    index.id,
                    &gin_hashes,
                    &new_pks,
                )
                .await?;
            continue;
        }

        if !index_helpers::is_index_materializable(index) {
            continue;
        }
        let new_matches = index_helpers::eval_index_predicate(index, schema, &new_row)?;
        if new_matches {
            let new_idx =
                index_helpers::get_index_values_with_expressions(index, schema, &new_row)?;
            let create_result = store
                .create_index_entry(
                    txn,
                    db_id,
                    schema.table_id,
                    index.id,
                    &new_idx,
                    &new_pks,
                    index.unique,
                )
                .await;
            if let Err(e) = create_result {
                if is_unique_duplicate_error(&e) {
                    if index.unique
                        && matches!(
                            index.state,
                            IndexState::Building | IndexState::WriteOnly | IndexState::Ready
                        )
                    {
                        match resolve_unique_index_conflict(
                            store, txn, db_id, schema, index, &new_idx, &new_pks,
                        )
                        .await?
                        {
                            UniqueConflictResolution::Idempotent
                            | UniqueConflictResolution::StaleReplaced => continue,
                            UniqueConflictResolution::RealConflict => {}
                        }
                    }

                    let cols = index.columns.join(", ");
                    let vals: Vec<String> = new_idx.iter().map(|v| format!("{}", v)).collect();
                    return Err(SqlError::UniqueViolation {
                        constraint: index.name.clone(),
                        message: format!(
                            "duplicate key value violates unique constraint \"{}\"\nDETAIL:  Key ({})=({}) already exists.",
                            index.name, cols, vals.join(", ")
                        ),
                        row_offset: None,
                    }
                    .into());
                }
                return Err(e);
            }
        }
    }

    maintain_hnsw_indexes_after_update(txn, db_id, schema, &new_row, &new_pks).await?;

    if propagate_fk_update {
        let fk_store_ctx = FkStoreCtx { store, db_id };
        handle_foreign_key_on_update(
            &fk_store_ctx,
            txn,
            table_name,
            schema,
            old_row,
            &new_row,
            fk_ctx,
        )
        .await?;
    }
    Ok(new_row)
}

/// Maintain HNSW indexes after UPDATE.
///
/// Loads the graph fresh from TiKV (committed state), adds the new vector
/// with the new PK label, serializes, and writes back to the transaction
/// buffer. usearch 0.21 has no `remove` method, so the old label (if PK
/// changed) stays in the graph — the HnswScanOperator's over-fetch strategy
/// filters it out via batch_get_rows visibility. For same-PK updates, the
/// new `add` overwrites the old vector at the same label.
async fn maintain_hnsw_indexes_after_update(
    txn: &mut Transaction,
    db_id: u64,
    schema: &TableSchema,
    new_row: &Row,
    new_pk_values: &[Value],
) -> Result<()> {
    if !schema.indexes.iter().any(|idx| idx.is_hnsw()) {
        return Ok(());
    }

    let pk_label = hnsw_pk_label(new_pk_values)?;

    for index in &schema.indexes {
        if !index.is_hnsw() {
            continue;
        }
        if matches!(index.state, IndexState::Invalid | IndexState::Building) {
            continue;
        }

        let Some(vector_col_name) = index.columns.first() else {
            return Err(anyhow::anyhow!(
                "HNSW index '{}' has no indexed column",
                index.name
            ));
        };
        let vector_col_idx = schema
            .column_index(vector_col_name)
            .ok_or_else(|| anyhow::anyhow!("HNSW index '{}' column not found", index.name))?;

        let vector_f64 = match new_row.values.get(vector_col_idx) {
            Some(Value::Null) | None => continue,
            Some(Value::Vector(v)) => v,
            Some(other) => {
                return Err(anyhow::anyhow!(
                    "HNSW index '{}' requires vector value, found {}",
                    index.name,
                    other.type_display_name()
                ))
            }
        };
        let vector_f32 = vec_f64_to_f32(vector_f64);
        let vector_dimensions = match schema.columns.get(vector_col_idx).map(|c| &c.data_type) {
            Some(DataType::Vector(dim)) => usize::try_from(*dim).map_err(|_| {
                anyhow::anyhow!(
                    "HNSW index '{}' vector dimension {} exceeds platform limits",
                    index.name,
                    dim
                )
            })?,
            _ => {
                return Err(anyhow::anyhow!(
                    "HNSW index '{}' column '{}' is not a vector type",
                    index.name,
                    vector_col_name
                ))
            }
        };

        // Load graph from the current DML transaction so that multi-row
        // UPDATEs accumulate: row N+1 sees row N's graph write via the
        // txn's local buffer (txn.get checks buffer before TiKV).
        let (hnsw_index, mut meta) =
            match load_hnsw_graph_from_txn(txn, db_id, schema.table_id, index.id).await? {
                Some(existing) => existing,
                None => {
                    let distance_metric = index.hnsw_distance_metric.as_deref().unwrap_or("l2");
                    let m = usize::from(index.hnsw_m.unwrap_or(HNSW_DEFAULT_M as u16));
                    let ef_construction = usize::from(
                        index
                            .hnsw_ef_construction
                            .unwrap_or(HNSW_DEFAULT_EF_CONSTRUCTION as u16),
                    );
                    create_empty_hnsw_index(vector_dimensions, distance_metric, m, ef_construction)
                        .map_err(|e| {
                            anyhow::anyhow!(
                                "failed to initialize HNSW graph for index '{}': {}",
                                index.name,
                                e
                            )
                        })?
                }
            };

        if meta.count >= (meta.capacity.saturating_mul(80) / 100) {
            let next_capacity = meta.capacity.saturating_mul(2).max(1);
            hnsw_index
                .reserve(next_capacity as usize)
                .map_err(|e| anyhow::anyhow!("failed to grow HNSW capacity: {}", e))?;
            meta.capacity = next_capacity;
        }

        hnsw_index
            .add(pk_label, &vector_f32)
            .map_err(|e| anyhow::anyhow!("failed to add vector to HNSW index: {}", e))?;
        // Use size() instead of blindly incrementing — same-PK overwrites
        // don't increase the vector count, only new labels do.
        meta.count = hnsw_index.size() as u64;

        let (graph_bytes, meta_bytes) =
            serialize_hnsw_snapshot(db_id, schema.table_id, index.id, &hnsw_index, &meta).map_err(
                |e| anyhow::anyhow!("failed to persist HNSW graph '{}': {}", index.name, e),
            )?;

        txn_put(
            txn,
            hnsw_graph_key(db_id, schema.table_id, index.id),
            graph_bytes,
        )
        .await?;
        txn_put(
            txn,
            hnsw_meta_key(db_id, schema.table_id, index.id),
            meta_bytes,
        )
        .await?;
    }

    Ok(())
}
