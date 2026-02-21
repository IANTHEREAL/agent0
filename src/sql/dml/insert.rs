//! INSERT row execution: conflict resolution, enum validation, and index materialization.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use tikv_client::Transaction;

use crate::sql::error::SqlError;
use crate::sql::gin::extract_gin_token_hashes_from_row;
use crate::sql::index_consistency::{
    is_unique_duplicate_error, resolve_unique_index_conflict, UniqueConflictResolution,
};
use crate::sql::index_helpers;
use crate::storage::TikvStore;
use crate::types::{DataType, Row, TableSchema, Value};
use crate::worker::types::IndexState;

use super::defaults::coerce_row_values;
use super::{ConflictBehavior, EnumLabelCache, InsertRowResult};

pub async fn build_enum_label_cache(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    schema: &TableSchema,
) -> Result<EnumLabelCache> {
    let mut required_types: HashSet<&str> = HashSet::new();
    for col in &schema.columns {
        if let DataType::UserDefined(udt_name) = &col.data_type {
            required_types.insert(udt_name.as_str());
        }
    }
    if required_types.is_empty() {
        return Ok(HashMap::new());
    }

    let mut cache: EnumLabelCache = HashMap::new();
    for udt_name in required_types {
        let def = store
            .get_type(txn, db_id, udt_name)
            .await?
            .ok_or_else(|| anyhow!("Type '{}' does not exist", udt_name))?;
        match def.kind {
            crate::types::UserTypeKind::Enum { labels } => {
                cache.insert(udt_name.to_string(), labels.into_iter().collect());
            }
            crate::types::UserTypeKind::Composite { .. } => {}
        }
    }

    Ok(cache)
}

pub(crate) fn validate_enum_values(
    schema: &TableSchema,
    row: &Row,
    cache: &EnumLabelCache,
) -> Result<()> {
    if cache.is_empty() {
        return Ok(());
    }

    for (idx, col) in schema.columns.iter().enumerate() {
        let DataType::UserDefined(udt_name) = &col.data_type else {
            continue;
        };

        let value = row
            .values
            .get(idx)
            .ok_or_else(|| anyhow!("Row value missing for column '{}'", col.name))?;
        if matches!(value, Value::Null) {
            continue;
        }

        // Non-enum UDTs (e.g. composite types) do not participate in enum-label
        // validation on write.
        let Some(labels) = cache.get(udt_name) else {
            continue;
        };

        let bare_type = udt_name.rsplit('.').next().unwrap_or(udt_name);
        match value {
            Value::Text(s) => {
                if !labels.contains(s) {
                    return Err(anyhow!(
                        "invalid input value for enum {}: \"{}\"",
                        bare_type,
                        s
                    ));
                }
            }
            other => {
                return Err(anyhow!(
                    "invalid input value for enum {}: {}",
                    bare_type,
                    other
                ));
            }
        }
    }

    Ok(())
}

pub async fn execute_insert_row(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    table_name: &str,
    schema: &TableSchema,
    row: Row,
    on_conflict: ConflictBehavior,
    enum_cache: &EnumLabelCache,
) -> Result<InsertRowResult> {
    let mut row_values = row.values;
    coerce_row_values(schema, &mut row_values)?;
    let row = Row::new(row_values);

    validate_enum_values(schema, &row, enum_cache)?;

    let pk_types: Vec<DataType> = if schema.pk_indices.is_empty() {
        vec![DataType::Uuid]
    } else {
        schema
            .pk_indices
            .iter()
            .map(|&idx| schema.columns[idx].data_type.clone())
            .collect()
    };

    if !schema.foreign_keys.is_empty() {
        super::foreign_keys::validate_foreign_keys(store, txn, db_id, schema, &row).await?;
    }

    let insert_result = store.insert(txn, db_id, table_name, row.clone()).await;
    match insert_result {
        Ok(pk_values) => {
            let mut created_index_entries: Vec<(u64, Vec<Value>, bool)> = Vec::new();
            for index in &schema.indexes {
                if matches!(index.state, IndexState::Building | IndexState::Invalid) {
                    continue;
                }
                if !index_helpers::is_index_materializable(index) {
                    continue;
                }
                if !index_helpers::eval_index_predicate(index, schema, &row)? {
                    continue;
                }
                let idx_values =
                    index_helpers::get_index_values_with_expressions(index, schema, &row)?;
                let result = store
                    .create_index_entry(
                        txn,
                        db_id,
                        schema.table_id,
                        index.id,
                        &idx_values,
                        &pk_values,
                        index.unique,
                    )
                    .await;
                if let Err(e) = result {
                    if is_unique_duplicate_error(&e) {
                        if index.unique
                            && matches!(index.state, IndexState::WriteOnly | IndexState::Ready)
                        {
                            match resolve_unique_index_conflict(
                                store,
                                txn,
                                db_id,
                                schema,
                                index,
                                &idx_values,
                                &pk_values,
                            )
                            .await?
                            {
                                UniqueConflictResolution::Idempotent
                                | UniqueConflictResolution::StaleReplaced => {
                                    created_index_entries.push((
                                        index.id,
                                        idx_values,
                                        index.unique,
                                    ));
                                    continue;
                                }
                                UniqueConflictResolution::RealConflict => {}
                            }
                        }
                        match on_conflict {
                            ConflictBehavior::DoNothing => {
                                rollback_inserted_index_entries(
                                    store,
                                    txn,
                                    db_id,
                                    schema,
                                    &pk_values,
                                    &created_index_entries,
                                )
                                .await?;
                                store
                                    .delete_by_pk(txn, db_id, table_name, &pk_values)
                                    .await?;
                                return Ok(InsertRowResult::Skipped);
                            }
                            ConflictBehavior::DoUpdate => {
                                let pks = store
                                    .scan_index(
                                        txn,
                                        db_id,
                                        schema.table_id,
                                        index.id,
                                        &idx_values,
                                        true,
                                        &pk_types,
                                        None,
                                    )
                                    .await?;
                                if pks.is_empty() {
                                    return Err(anyhow!(
                                        "Failed to find conflicting row in unique index"
                                    ));
                                }
                                let existing_pk = pks.into_iter().next().unwrap();

                                let existing_rows = store
                                    .batch_get_rows(
                                        txn,
                                        db_id,
                                        schema.table_id,
                                        vec![existing_pk.clone()],
                                        schema,
                                    )
                                    .await?;
                                let existing_row =
                                    existing_rows.into_iter().next().ok_or_else(|| {
                                        anyhow!("Failed to fetch existing row for upsert")
                                    })?;

                                rollback_inserted_index_entries(
                                    store,
                                    txn,
                                    db_id,
                                    schema,
                                    &pk_values,
                                    &created_index_entries,
                                )
                                .await?;
                                store
                                    .delete_by_pk(txn, db_id, table_name, &pk_values)
                                    .await?;

                                return Ok(InsertRowResult::Conflicted {
                                    existing_pk,
                                    existing_row,
                                    excluded_row: row,
                                });
                            }
                            ConflictBehavior::Error => {
                                let cols = index.columns.join(", ");
                                let vals: Vec<String> =
                                    idx_values.iter().map(|v| format!("{}", v)).collect();
                                return Err(SqlError::UniqueViolation {
                                    constraint: index.name.clone(),
                                    message: format!(
                                        "duplicate key value violates unique constraint \"{}\"\nDETAIL:  Key ({})=({}) already exists.",
                                        index.name, cols, vals.join(", ")
                                    ),
                                }.into());
                            }
                        }
                    }
                    return Err(e);
                }
                created_index_entries.push((index.id, idx_values, index.unique));
            }
            // Materialize supported GIN indexes only after B-Tree indexes succeed, so
            // ON CONFLICT paths don't need additional cleanup.
            for index in &schema.indexes {
                if matches!(index.state, IndexState::Building | IndexState::Invalid) {
                    continue;
                }
                let hashes = extract_gin_token_hashes_from_row(schema, index, &row)?;
                if hashes.is_empty() {
                    continue;
                }
                store
                    .create_gin_index_entries(
                        txn,
                        db_id,
                        schema.table_id,
                        index.id,
                        &hashes,
                        &pk_values,
                    )
                    .await?;
            }
            Ok(InsertRowResult::Inserted(row))
        }
        Err(e)
            if e.downcast_ref::<SqlError>()
                .is_some_and(|se| matches!(se, SqlError::UniqueViolation { .. })) =>
        {
            if schema.pk_indices.is_empty() {
                return Err(e);
            }
            let pk_values = schema.get_pk_values(&row);
            match on_conflict {
                ConflictBehavior::DoNothing => Ok(InsertRowResult::Skipped),
                ConflictBehavior::DoUpdate => {
                    let existing_rows = store
                        .batch_get_rows(
                            txn,
                            db_id,
                            schema.table_id,
                            vec![pk_values.clone()],
                            schema,
                        )
                        .await?;
                    let existing_row = existing_rows
                        .into_iter()
                        .next()
                        .ok_or_else(|| anyhow!("Failed to fetch existing row for upsert"))?;
                    Ok(InsertRowResult::Conflicted {
                        existing_pk: pk_values,
                        existing_row,
                        excluded_row: row,
                    })
                }
                ConflictBehavior::Error => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}

async fn rollback_inserted_index_entries(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    schema: &TableSchema,
    pk_values: &[Value],
    created_index_entries: &[(u64, Vec<Value>, bool)],
) -> Result<()> {
    for (index_id, idx_values, unique) in created_index_entries.iter().rev() {
        store
            .delete_index_entry(
                txn,
                db_id,
                schema.table_id,
                *index_id,
                idx_values,
                pk_values,
                *unique,
            )
            .await?;
    }
    Ok(())
}
