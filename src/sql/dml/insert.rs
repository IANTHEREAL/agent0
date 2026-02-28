//! INSERT row execution: conflict resolution, enum validation, and index materialization.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use tikv_client::Transaction;

use crate::model::{DataType, IndexDef, Row, TableSchema, Value};
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
/// Loads the graph fresh from TiKV (committed state), adds the new vector,
/// serializes, and writes back to the transaction buffer. If the enclosing
/// transaction rolls back, TiKV never received the write and the next load
/// sees the old graph. If it commits, the next load gets the new graph.
async fn maintain_hnsw_indexes_after_insert(
    txn: &mut Transaction,
    db_id: u64,
    schema: &TableSchema,
    row: &Row,
    pk_values: &[Value],
) -> Result<()> {
    if !schema.indexes.iter().any(|idx| idx.is_hnsw()) {
        return Ok(());
    }

    let pk_label = hnsw_pk_label(pk_values)?;

    for index in &schema.indexes {
        if !index.is_hnsw() {
            continue;
        }
        if matches!(index.state, IndexState::Invalid | IndexState::Building) {
            continue;
        }

        let Some(vector_col_name) = index.columns.first() else {
            return Err(anyhow!("HNSW index '{}' has no indexed column", index.name));
        };
        let vector_col_idx = schema
            .column_index(vector_col_name)
            .ok_or_else(|| anyhow!("HNSW index '{}' column not found", index.name))?;

        let vector_f64 = match row.values.get(vector_col_idx) {
            Some(Value::Null) | None => continue,
            Some(Value::Vector(v)) => v,
            Some(other) => {
                return Err(anyhow!(
                    "HNSW index '{}' requires vector value, found {}",
                    index.name,
                    other.type_display_name()
                ))
            }
        };
        let vector_f32 = vec_f64_to_f32(vector_f64);
        let vector_dimensions = match schema.columns.get(vector_col_idx).map(|c| &c.data_type) {
            Some(DataType::Vector(dim)) => usize::try_from(*dim).map_err(|_| {
                anyhow!(
                    "HNSW index '{}' vector dimension {} exceeds platform limits",
                    index.name,
                    dim
                )
            })?,
            _ => {
                return Err(anyhow!(
                    "HNSW index '{}' column '{}' is not a vector type",
                    index.name,
                    vector_col_name
                ))
            }
        };

        // Load graph from the current DML transaction so that multi-row
        // INSERTs accumulate: row N+1 sees row N's graph write via the
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
                            anyhow!(
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
                .map_err(|e| anyhow!("failed to grow HNSW capacity: {}", e))?;
            meta.capacity = next_capacity;
        }

        hnsw_index
            .add(pk_label, &vector_f32)
            .map_err(|e| anyhow!("failed to add vector to HNSW index: {}", e))?;
        // Use size() for accurate count — re-inserted PK labels (after
        // DELETE left a stale entry) are overwrites, not new entries.
        meta.count = hnsw_index.size() as u64;

        let (graph_bytes, meta_bytes) =
            serialize_hnsw_snapshot(db_id, schema.table_id, index.id, &hnsw_index, &meta)
                .map_err(|e| anyhow!("failed to persist HNSW graph '{}': {}", index.name, e))?;

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

        match (&col.data_type, value) {
            // Scalar enum: validate single label.
            (DataType::UserDefined(_), Value::Text(s)) => {
                if !labels.contains(s) {
                    return Err(anyhow!(
                        "invalid input value for enum {}: \"{}\"",
                        bare_type,
                        s
                    ));
                }
            }
            // Enum array: validate each element label.
            (DataType::Array(_), Value::Array(elements)) => {
                for elem in elements {
                    match elem {
                        Value::Null => {} // NULL elements are valid in PG
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
            }
            (_, other) => {
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
            maintain_hnsw_indexes_after_insert(txn, db_id, schema, &row, &pk_values).await?;
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
        TableSchema {
            name: "public.t_conflict".to_string(),
            table_id: 1,
            columns: vec![
                ColumnDef {
                    name: "id".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: true,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    collation: None,
                },
                ColumnDef {
                    name: "email".to_string(),
                    data_type: DataType::Text,
                    nullable: false,
                    primary_key: false,
                    unique: true,
                    is_serial: false,
                    default_expr: None,
                    collation: None,
                },
            ],
            version: 1,
            pk_constraint_name: Some("t_conflict_pkey".to_string()),
            pk_indices: vec![0],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: "postgres".to_string(),
            from_alias: None,
        }
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
        TableSchema {
            name: "public.t_enum_arr".to_string(),
            table_id: 2,
            columns: vec![ColumnDef {
                name: "moods".to_string(),
                data_type: DataType::Array(Box::new(DataType::UserDefined("public.mood".into()))),
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            }],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: "postgres".to_string(),
            from_alias: None,
        }
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
