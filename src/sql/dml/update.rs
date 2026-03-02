//! UPDATE row execution: index maintenance, PK change detection, and row upsert.

use std::sync::Arc;

use anyhow::Result;
use tikv_client::Transaction;

use crate::model::{DataType, Row, TableSchema, Value};
use crate::sql::error::SqlError;
use crate::sql::gin::extract_gin_token_hashes_from_row;
use crate::sql::hnsw::hnsw_pk_label;
use crate::sql::hnsw::storage::{hnsw_meta_key, write_hnsw_deltas};
use crate::sql::hnsw::vec_f64_to_f32;
use crate::sql::index_consistency::{
    is_unique_duplicate_error, resolve_unique_index_conflict, UniqueConflictResolution,
};
use crate::sql::index_helpers;
use crate::storage::TikvStore;
use crate::worker::types::IndexState;

/// Stats returned by batch HNSW maintenance for observability.
pub struct HnswBatchStats {
    /// Total graph bytes written to TiKV.
    pub graph_bytes: u64,
    /// Total serialization time in microseconds.
    pub serialize_duration_us: u64,
    /// Number of delta entries written (for merge trigger).
    pub delta_count: usize,
    /// Index IDs that actually received delta writes (for targeted merge enqueue).
    pub dirty_index_ids: Vec<u64>,
}

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
    execute_update_row_by_pk_inner(
        store, txn, db_id, table_name, schema, pk_values, old_row, new_row, enum_cache, false,
    )
    .await
}

/// Like [`execute_update_row_by_pk`] but defers HNSW index maintenance.
///
/// The caller is responsible for calling [`batch_maintain_hnsw_indexes`]
/// after all rows in the statement have been updated.
pub async fn execute_update_row_by_pk_defer_hnsw(
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
    execute_update_row_by_pk_inner(
        store, txn, db_id, table_name, schema, pk_values, old_row, new_row, enum_cache, true,
    )
    .await
}

async fn execute_update_row_by_pk_inner(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    table_name: &str,
    schema: &TableSchema,
    pk_values: &[Value],
    old_row: &Row,
    new_row: Row,
    enum_cache: &EnumLabelCache,
    skip_hnsw: bool,
) -> Result<Row> {
    let mut new_row_values = new_row.values;
    coerce_row_values(schema, &mut new_row_values)?;
    let new_row = Row::new(new_row_values);

    validate_enum_values(schema, &new_row, enum_cache)?;
    if !schema.foreign_keys.is_empty() {
        validate_foreign_keys(store, txn, db_id, schema, &new_row).await?;
    }

    update_row_indexes(
        store, txn, db_id, schema, pk_values, old_row, &new_row, skip_hnsw,
    )
    .await?;
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
    skip_hnsw: bool,
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

    if !skip_hnsw {
        maintain_hnsw_indexes_after_update(
            txn, db_id, schema, old_row, new_row, pk_values, pk_values,
        )
        .await?;
    }
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
        false,
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
        false,
    )
    .await
}

/// Like [`execute_update_row`] but defers HNSW index maintenance.
///
/// The caller is responsible for calling [`batch_maintain_hnsw_indexes`]
/// after all rows in the statement have been updated.  This avoids
/// per-row graph load/serialize/write, reducing the cost from
/// O(updated_rows * graph_size) to O(graph_size + updated_rows).
pub async fn execute_update_row_defer_hnsw(
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
        true,
    )
    .await
}

/// Batch-maintain all HNSW indexes after a multi-row UPDATE.
///
/// Loads each HNSW graph ONCE, adds all changed vectors, then serializes
/// and writes ONCE.  This replaces the per-row load/add/serialize/write
/// loop that was the primary I/O amplifier under concurrent UPDATE
/// workloads (see issue #1284).
///
/// `changes` contains `(old_row, new_row)` pairs for every row that was
/// updated in the statement.  Rows whose vector column is unchanged are
/// filtered internally — callers may pass all updated rows.
pub async fn batch_maintain_hnsw_indexes(
    txn: &mut Transaction,
    db_id: u64,
    schema: &TableSchema,
    changes: &[(Row, Row)],
) -> Result<HnswBatchStats> {
    if changes.is_empty() || !schema.indexes.iter().any(|idx| idx.is_hnsw()) {
        return Ok(HnswBatchStats {
            graph_bytes: 0,
            serialize_duration_us: 0,
            delta_count: 0,
            dirty_index_ids: Vec::new(),
        });
    }
    let mut stats = HnswBatchStats {
        graph_bytes: 0,
        serialize_duration_us: 0,
        delta_count: 0,
        dirty_index_ids: Vec::new(),
    };

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
        let _vector_dimensions = match schema.columns.get(vector_col_idx).map(|c| &c.data_type) {
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

        // Collect vectors that actually changed (skip unchanged vector + PK).
        let mut pending: Vec<(u64, Vec<f32>)> = Vec::new();
        for (old_row, new_row) in changes {
            let old_pk_values = schema.get_pk_values(old_row);
            let new_pk_values = schema.get_pk_values(new_row);
            if old_pk_values == new_pk_values
                && old_row.values.get(vector_col_idx) == new_row.values.get(vector_col_idx)
            {
                continue;
            }
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
            let pk_label = hnsw_pk_label(&new_pk_values)?;
            pending.push((pk_label, vec_f64_to_f32(vector_f64)));
        }

        if pending.is_empty() {
            continue;
        }

        // Validate storage version (only v1 delta-log supported).
        let meta_key = hnsw_meta_key(db_id, schema.table_id, index.id);
        let meta_bytes_opt = txn
            .get(meta_key.clone())
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        if let Some(meta_bytes) = meta_bytes_opt {
            let meta: crate::sql::hnsw::HnswMeta =
                serde_json::from_slice(&meta_bytes).map_err(|e| anyhow::anyhow!(e))?;
            if meta.storage_version != 1 {
                return Err(anyhow::anyhow!(
                    "HNSW index '{}' has unsupported storage_version={}; please rebuild",
                    index.name,
                    meta.storage_version
                ));
            }
        }
        // else: no meta means CREATE INDEX hasn't finished; write deltas anyway
        // (they'll be applied on first read after the index is created).

        // Write delta entries (unique keys, ~100B each, zero shared-key contention).
        let delta_bytes = write_hnsw_deltas(txn, db_id, schema.table_id, index.id, &pending)
            .await
            .map_err(|e| {
                anyhow::anyhow!("failed to write HNSW deltas for '{}': {}", index.name, e)
            })?;
        stats.graph_bytes += delta_bytes;
        stats.delta_count += pending.len();
        stats.dirty_index_ids.push(index.id);
    }

    Ok(stats)
}

/// Batch-maintain all HNSW indexes after a multi-row INSERT.
///
/// Same load-once/serialize-once strategy as [`batch_maintain_hnsw_indexes`]
/// but for inserted rows (no old_row comparison needed).
pub async fn batch_maintain_hnsw_indexes_for_inserts(
    txn: &mut Transaction,
    db_id: u64,
    schema: &TableSchema,
    inserted_rows: &[Row],
) -> Result<HnswBatchStats> {
    if inserted_rows.is_empty() || !schema.indexes.iter().any(|idx| idx.is_hnsw()) {
        return Ok(HnswBatchStats {
            graph_bytes: 0,
            serialize_duration_us: 0,
            delta_count: 0,
            dirty_index_ids: Vec::new(),
        });
    }
    let mut stats = HnswBatchStats {
        graph_bytes: 0,
        serialize_duration_us: 0,
        delta_count: 0,
        dirty_index_ids: Vec::new(),
    };

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
        let _vector_dimensions = match schema.columns.get(vector_col_idx).map(|c| &c.data_type) {
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

        // Collect vectors from inserted rows (skip NULLs).
        let mut pending: Vec<(u64, Vec<f32>)> = Vec::new();
        for row in inserted_rows {
            let vector_f64 = match row.values.get(vector_col_idx) {
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
            let pk_values = schema.get_pk_values(row);
            let pk_label = hnsw_pk_label(&pk_values)?;
            pending.push((pk_label, vec_f64_to_f32(vector_f64)));
        }

        if pending.is_empty() {
            continue;
        }

        // Validate storage version (only v1 delta-log supported).
        let meta_key = hnsw_meta_key(db_id, schema.table_id, index.id);
        let meta_bytes_opt = txn
            .get(meta_key.clone())
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        if let Some(meta_bytes) = meta_bytes_opt {
            let meta: crate::sql::hnsw::HnswMeta =
                serde_json::from_slice(&meta_bytes).map_err(|e| anyhow::anyhow!(e))?;
            if meta.storage_version != 1 {
                return Err(anyhow::anyhow!(
                    "HNSW index '{}' has unsupported storage_version={}; please rebuild",
                    index.name,
                    meta.storage_version
                ));
            }
        }

        // Write delta entries.
        let delta_bytes = write_hnsw_deltas(txn, db_id, schema.table_id, index.id, &pending)
            .await
            .map_err(|e| {
                anyhow::anyhow!("failed to write HNSW deltas for '{}': {}", index.name, e)
            })?;
        stats.graph_bytes += delta_bytes;
        stats.delta_count += pending.len();
        stats.dirty_index_ids.push(index.id);
    }

    Ok(stats)
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
    skip_hnsw: bool,
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

    if !skip_hnsw {
        maintain_hnsw_indexes_after_update(
            txn, db_id, schema, old_row, &new_row, &old_pks, &new_pks,
        )
        .await?;
    }

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
/// Loads the graph from TiKV, adds the new vector with the new PK label,
/// serializes, and writes back to the transaction buffer.
///
/// Skips the graph write entirely when the vector column is unchanged
/// (e.g. `UPDATE t SET non_vector_col = ...`), avoiding unnecessary
/// serialization round-trips and duplicate label accumulation.
///
/// usearch 0.21 `add()` always appends — it does NOT overwrite an
/// existing label. When the PK is unchanged, the old label stays in the
/// graph; the HnswScanOperator's over-fetch strategy filters stale
/// entries via batch_get_rows visibility.
async fn maintain_hnsw_indexes_after_update(
    txn: &mut Transaction,
    db_id: u64,
    schema: &TableSchema,
    old_row: &Row,
    new_row: &Row,
    old_pk_values: &[Value],
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

        // Skip HNSW maintenance when both the PK and vector column are
        // unchanged.  The HNSW label is derived from the PK, so a PK
        // change requires adding a new label even if the vector is the
        // same.  We compare values directly rather than using
        // index_values_unchanged(), which only works for btree indexes.
        if old_pk_values == new_pk_values
            && old_row.values.get(vector_col_idx) == new_row.values.get(vector_col_idx)
        {
            continue;
        }

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

        // Validate storage version (only v1 delta-log supported).
        let meta_key = hnsw_meta_key(db_id, schema.table_id, index.id);
        let meta_bytes_opt = txn
            .get(meta_key.clone())
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        if let Some(meta_bytes) = meta_bytes_opt {
            let meta: crate::sql::hnsw::HnswMeta =
                serde_json::from_slice(&meta_bytes).map_err(|e| anyhow::anyhow!(e))?;
            if meta.storage_version != 1 {
                return Err(anyhow::anyhow!(
                    "HNSW index '{}' has unsupported storage_version={}; please rebuild",
                    index.name,
                    meta.storage_version
                ));
            }
        }

        // Write single delta entry.
        let adds = vec![(pk_label, vector_f32)];
        write_hnsw_deltas(txn, db_id, schema.table_id, index.id, &adds)
            .await
            .map_err(|e| {
                anyhow::anyhow!("failed to write HNSW delta for '{}': {}", index.name, e)
            })?;
    }

    Ok(())
}
