//! UPDATE row execution: index maintenance, PK change detection, and row upsert.

use std::sync::Arc;

use anyhow::Result;
use tikv_client::Transaction;

use crate::sql::error::SqlError;
use crate::sql::gin::extract_gin_token_hashes_from_row;
use crate::sql::index_consistency::{
    is_unique_duplicate_error, resolve_unique_index_conflict, UniqueConflictResolution,
};
use crate::sql::index_helpers;
use crate::storage::TikvStore;
use crate::types::{Row, TableSchema, Value};
use crate::worker::types::IndexState;

use super::defaults::coerce_row_values;
use super::foreign_keys::{handle_foreign_key_on_update, validate_foreign_keys};
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
        if matches!(index.state, IndexState::Building | IndexState::Invalid) {
            continue;
        }
        if index_helpers::index_values_unchanged(index, schema, old_row, new_row)? {
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
                        &pk_values,
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
                        &pk_values,
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
                    &pk_values,
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
                    &pk_values,
                    index.unique,
                )
                .await;
            if let Err(e) = create_result {
                if index.unique
                    && matches!(index.state, IndexState::WriteOnly | IndexState::Ready)
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
) -> Result<Row> {
    let mut new_row_values = new_row.values;
    coerce_row_values(schema, &mut new_row_values)?;
    let new_row = Row::new(new_row_values);

    validate_enum_values(schema, &new_row, enum_cache)?;

    let old_pks = schema.get_pk_values(old_row);
    let new_pks = schema.get_pk_values(&new_row);
    let pk_changed = old_pks != new_pks;

    if !schema.foreign_keys.is_empty() {
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
            }.into());
        }
    }

    for index in &schema.indexes {
        if matches!(index.state, IndexState::Building | IndexState::Invalid) {
            continue;
        }
        if !pk_changed && index_helpers::index_values_unchanged(index, schema, old_row, &new_row)? {
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
        if matches!(index.state, IndexState::Building | IndexState::Invalid) {
            continue;
        }
        if !pk_changed && index_helpers::index_values_unchanged(index, schema, old_row, &new_row)? {
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
                        && matches!(index.state, IndexState::WriteOnly | IndexState::Ready)
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
                    }
                    .into());
                }
                return Err(e);
            }
        }
    }

    handle_foreign_key_on_update(store, txn, db_id, table_name, schema, old_row, &new_row).await?;
    Ok(new_row)
}
