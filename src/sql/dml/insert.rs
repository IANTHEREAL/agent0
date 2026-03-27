//! INSERT row execution: conflict resolution, enum validation, and index materialization.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use tikv_client::Transaction;

use crate::model::{DataType, IndexDef, Row, TableSchema, Value};
use crate::sql::error::SqlError;
use crate::sql::gin::extract_gin_token_hashes_from_row;
use crate::sql::index_consistency::{
    is_unique_duplicate_error, resolve_unique_index_conflict, UniqueConflictResolution,
};
use crate::sql::index_helpers;
use crate::storage::TikvStore;
use crate::worker::types::IndexState;

use super::defaults::coerce_row_values;
use super::{ConflictBehavior, ConflictTarget, EnumLabelCache, InsertRowResult};

/// Check if an index matches the ON CONFLICT target.
/// If target is None, any unique index matches (legacy behavior).
fn index_matches_conflict_target(
    index: &IndexDef,
    _schema: &TableSchema,
    target: Option<&ConflictTarget>,
) -> bool {
    match target {
        None => true,
        Some(ConflictTarget::Columns(targets)) => {
            if !index.unique {
                return false;
            }
            // Match if the index columns are exactly the target columns (order-independent).
            if index.columns.len() != targets.len() {
                return false;
            }
            targets.iter().all(|t| index.columns.iter().any(|c| c == t))
        }
        Some(ConflictTarget::Constraint(name)) => index.unique && index.name == *name,
    }
}

/// Check if the primary key matches the ON CONFLICT target.
/// If target is None, PK matches (legacy behavior).
fn pk_matches_conflict_target(schema: &TableSchema, target: Option<&ConflictTarget>) -> bool {
    match target {
        None => true,
        Some(ConflictTarget::Columns(targets)) => {
            let pk_col_names: Vec<&str> = schema
                .pk_indices
                .iter()
                .map(|&idx| schema.columns[idx].name.as_str())
                .collect();
            if pk_col_names.len() != targets.len() {
                return false;
            }
            targets.iter().all(|t| pk_col_names.contains(&t.as_str()))
        }
        Some(ConflictTarget::Constraint(name)) => schema
            .pk_constraint_name
            .as_deref()
            .is_some_and(|pk_name| pk_name == *name),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UniqueConflictPolicy {
    SkipRow,
    Upsert,
    DeferUniqueViolation,
    RaiseUniqueViolation,
}

fn unique_conflict_policy(
    on_conflict: &ConflictBehavior,
    index: &IndexDef,
    schema: &TableSchema,
) -> UniqueConflictPolicy {
    match on_conflict {
        ConflictBehavior::DoNothing => UniqueConflictPolicy::SkipRow,
        ConflictBehavior::DoUpdate { target } => {
            if index_matches_conflict_target(index, schema, target.as_ref()) {
                UniqueConflictPolicy::Upsert
            } else {
                UniqueConflictPolicy::DeferUniqueViolation
            }
        }
        ConflictBehavior::Error => UniqueConflictPolicy::RaiseUniqueViolation,
    }
}

fn build_unique_violation_error(index: &IndexDef, idx_values: &[Value]) -> SqlError {
    let cols = index.columns.join(", ");
    let vals: Vec<String> = idx_values.iter().map(|v| format!("{}", v)).collect();
    SqlError::UniqueViolation {
        constraint: index.name.clone(),
        message: format!(
            "duplicate key value violates unique constraint \"{}\"\nDETAIL:  Key ({})=({}) already exists.",
            index.name,
            cols,
            vals.join(", ")
        ),
        row_offset: None,
    }
}

/// Maintain HNSW indexes after INSERT.
///
/// Delegates to the shared HNSW delta-maintenance helper in
/// [`super::update::maintain_hnsw_indexes_inner`].
async fn maintain_hnsw_indexes_after_insert(
    txn: &mut Transaction,
    store: &TikvStore,
    db_id: u64,
    schema: &TableSchema,
    row: &Row,
    pk_values: &[Value],
) -> Result<()> {
    super::update::maintain_hnsw_indexes_inner(txn, store, db_id, schema, row, pk_values).await
}

pub async fn build_enum_label_cache(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    schema: &TableSchema,
) -> Result<EnumLabelCache> {
    let mut required_types: HashSet<&str> = HashSet::new();
    for col in &schema.columns {
        match &col.data_type {
            DataType::UserDefined(udt_name) => {
                required_types.insert(udt_name.as_str());
            }
            DataType::Array(inner) => {
                if let DataType::UserDefined(udt_name) = inner.as_ref() {
                    required_types.insert(udt_name.as_str());
                }
            }
            _ => {}
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
            crate::model::UserTypeKind::Enum { labels } => {
                cache.insert(udt_name.to_string(), labels.into_iter().collect());
            }
            crate::model::UserTypeKind::Composite { .. } => {}
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
        // Extract the UDT name from scalar enum or array-of-enum columns.
        let udt_name = match &col.data_type {
            DataType::UserDefined(name) => name,
            DataType::Array(inner) => match inner.as_ref() {
                DataType::UserDefined(name) => name,
                _ => continue,
            },
            _ => continue,
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
        crate::sql::udt::validate_enum_value_against_labels(
            &col.data_type,
            value,
            labels,
            bare_type,
        )?;
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
    fk_ref_cache: Option<&super::foreign_keys::FkRefSchemaCache>,
    fk_lock_cache: Option<&mut super::foreign_keys::FkLockCache>,
) -> Result<InsertRowResult> {
    execute_insert_row_inner(
        store,
        txn,
        db_id,
        table_name,
        schema,
        row,
        on_conflict,
        enum_cache,
        false,
        fk_ref_cache,
        fk_lock_cache,
    )
    .await
}

/// Like [`execute_insert_row`] but defers HNSW index maintenance.
///
/// The caller is responsible for calling
/// [`super::update::batch_maintain_hnsw_indexes_for_inserts`] after all
/// rows in the statement have been inserted.
pub async fn execute_insert_row_defer_hnsw(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    table_name: &str,
    schema: &TableSchema,
    row: Row,
    on_conflict: ConflictBehavior,
    enum_cache: &EnumLabelCache,
    fk_ref_cache: Option<&super::foreign_keys::FkRefSchemaCache>,
    fk_lock_cache: Option<&mut super::foreign_keys::FkLockCache>,
) -> Result<InsertRowResult> {
    execute_insert_row_inner(
        store,
        txn,
        db_id,
        table_name,
        schema,
        row,
        on_conflict,
        enum_cache,
        true,
        fk_ref_cache,
        fk_lock_cache,
    )
    .await
}

async fn execute_insert_row_inner(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    table_name: &str,
    schema: &TableSchema,
    row: Row,
    on_conflict: ConflictBehavior,
    enum_cache: &EnumLabelCache,
    skip_hnsw: bool,
    fk_ref_cache: Option<&super::foreign_keys::FkRefSchemaCache>,
    fk_lock_cache: Option<&mut super::foreign_keys::FkLockCache>,
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
        let owned_cache;
        let ref_cache = match fk_ref_cache {
            Some(c) => c,
            None => {
                owned_cache = super::foreign_keys::build_fk_ref_schema_cache(
                    store, txn, db_id, schema, false,
                )
                .await?;
                &owned_cache
            }
        };
        super::foreign_keys::validate_foreign_keys_with_cache(
            store, txn, db_id, schema, &row, ref_cache, fk_lock_cache,
        )
        .await?;
    }

    let insert_result = store.insert(txn, db_id, table_name, row.clone()).await;
    match insert_result {
        Ok(pk_values) => {
            let mut created_index_entries: Vec<(u64, Vec<Value>, bool)> = Vec::new();
            let mut deferred_unique_violation: Option<SqlError> = None;
            for index in &schema.indexes {
                if matches!(index.state, IndexState::Invalid)
                    || (matches!(index.state, IndexState::Building) && !index.unique)
                {
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
                            && matches!(
                                index.state,
                                IndexState::Building | IndexState::WriteOnly | IndexState::Ready
                            )
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
                        match unique_conflict_policy(&on_conflict, index, schema) {
                            UniqueConflictPolicy::SkipRow => {
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
                            UniqueConflictPolicy::Upsert => {
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
                            UniqueConflictPolicy::DeferUniqueViolation => {
                                if deferred_unique_violation.is_none() {
                                    deferred_unique_violation =
                                        Some(build_unique_violation_error(index, &idx_values));
                                }
                                continue;
                            }
                            UniqueConflictPolicy::RaiseUniqueViolation => {
                                return Err(build_unique_violation_error(index, &idx_values).into());
                            }
                        }
                    }
                    return Err(e);
                }
                created_index_entries.push((index.id, idx_values, index.unique));
            }
            if let Some(err) = deferred_unique_violation {
                return Err(err.into());
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
            if !skip_hnsw {
                maintain_hnsw_indexes_after_insert(txn, store, db_id, schema, &row, &pk_values)
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
            match &on_conflict {
                ConflictBehavior::DoNothing => Ok(InsertRowResult::Skipped),
                ConflictBehavior::DoUpdate { target }
                    if pk_matches_conflict_target(schema, target.as_ref()) =>
                {
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
                ConflictBehavior::Error | ConflictBehavior::DoUpdate { .. } => Err(e),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ColumnDef, DataType};
    use std::collections::HashMap;

    fn test_schema() -> TableSchema {
        TableSchema::new(
            "public.t_conflict".to_string(),
            1,
            vec![
                ColumnDef::new("id", DataType::Int32, false).primary_key(),
                ColumnDef::new("email", DataType::Text, false).unique(),
            ],
            vec![0],
        )
    }

    fn unique_index(name: &str, columns: &[&str]) -> IndexDef {
        IndexDef {
            name: name.to_string(),
            id: 1,
            columns: columns.iter().map(|c| (*c).to_string()).collect(),
            unique: true,
            is_constraint: false,
            method: None,
            predicate: None,
            expressions: vec![],
            state: IndexState::Ready,
            cached_predicate_conjuncts: None,
            hnsw_m: None,
            hnsw_ef_construction: None,
            hnsw_distance_metric: None,
        }
    }

    fn enum_array_schema() -> TableSchema {
        TableSchema::new(
            "public.t_enum_arr".to_string(),
            2,
            vec![ColumnDef::new(
                "moods",
                DataType::Array(Box::new(DataType::UserDefined("public.mood".into()))),
                false,
            )],
            vec![],
        )
    }

    fn enum_array_cache() -> EnumLabelCache {
        let mut cache = HashMap::new();
        cache.insert(
            "public.mood".to_string(),
            ["happy", "sad", "neutral"]
                .into_iter()
                .map(|s| s.to_string())
                .collect(),
        );
        cache
    }

    #[test]
    fn index_target_matches_constraint_name() {
        let schema = test_schema();
        let index = unique_index("uq_t_conflict_email", &["email"]);
        let target = ConflictTarget::Constraint("uq_t_conflict_email".to_string());
        assert!(index_matches_conflict_target(
            &index,
            &schema,
            Some(&target)
        ));
    }

    #[test]
    fn index_target_rejects_other_constraint_name() {
        let schema = test_schema();
        let index = unique_index("uq_t_conflict_email", &["email"]);
        let target = ConflictTarget::Constraint("other_constraint".to_string());
        assert!(!index_matches_conflict_target(
            &index,
            &schema,
            Some(&target)
        ));
    }

    #[test]
    fn pk_target_matches_pk_constraint_name() {
        let schema = test_schema();
        let target = ConflictTarget::Constraint("t_conflict_pkey".to_string());
        assert!(pk_matches_conflict_target(&schema, Some(&target)));
    }

    #[test]
    fn do_update_non_target_conflict_is_deferred_until_target_checked() {
        let schema = test_schema();
        let on_conflict = ConflictBehavior::DoUpdate {
            target: Some(ConflictTarget::Columns(vec!["email".to_string()])),
        };
        let first = unique_index("uq_t_conflict_other", &["id"]);
        let second = unique_index("uq_t_conflict_email", &["email"]);

        let mut deferred = false;
        let mut selected_upsert = false;
        for idx in [&first, &second] {
            match unique_conflict_policy(&on_conflict, idx, &schema) {
                UniqueConflictPolicy::DeferUniqueViolation => deferred = true,
                UniqueConflictPolicy::Upsert => {
                    selected_upsert = true;
                    break;
                }
                _ => {}
            }
        }

        assert!(deferred);
        assert!(selected_upsert);
    }

    #[test]
    fn enum_array_cast_keeps_invalid_variant_and_validation_errors() {
        let schema = enum_array_schema();
        let cache = enum_array_cache();
        let mut row_vals = vec![Value::Text("{happy,angry,sad}".to_string())];

        coerce_row_values(&schema, &mut row_vals).unwrap();

        let Value::Array(values) = &row_vals[0] else {
            panic!("enum array assignment cast did not produce Value::Array");
        };
        assert_eq!(values.len(), 3);
        assert!(values
            .iter()
            .any(|v| matches!(v, Value::Text(s) if s == "angry")));

        let err = validate_enum_values(&schema, &Row::new(row_vals), &cache).unwrap_err();
        assert!(err
            .to_string()
            .contains("invalid input value for enum mood: \"angry\""));
    }
}
