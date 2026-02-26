//! CREATE INDEX, index backfill, index state management, and index
//! reconciliation for CREATE INDEX CONCURRENTLY.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use sqlparser::ast::{Expr, OrderByExpr};
use tikv_client::Transaction;

use crate::model::{DataType, IndexDef, Row, TableSchema};
use crate::sql::error::SqlError;
use crate::sql::gin::{extract_gin_token_hashes_from_row, supported_gin_index_column};
use crate::sql::index_consistency::{
    is_unique_duplicate_error, pk_types_for_schema, resolve_unique_index_conflict,
    UniqueConflictResolution,
};
use crate::sql::index_helpers;
use crate::sql::names::normalize_ident;
use crate::sql::projection::fill_row_defaults;
use crate::sql::ExecuteResult;
use crate::storage::TikvStore;
use crate::txn::txn_delete;
use crate::worker::types::{IndexState, TaskQueueEntry, TaskType, TASK_TYPE_BG_DDL};

use super::create_table::check_relation_name_available;
use super::{
    analyze_row_level_expr, delete_range, index_prefix_range, maybe_rotate_backfill_txn,
    KvScanBatches, DDL_SCAN_BATCH_SIZE,
};

pub async fn execute_create_index(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    idx_name: &str,
    table_name: &str,
    using: Option<&sqlparser::ast::Ident>,
    columns: &[OrderByExpr],
    unique: bool,
    if_not_exists: bool,
    concurrently: bool,
    predicate: Option<&Expr>,
    rows: Vec<Row>,
    keyspace: &str,
    username: &str,
) -> Result<ExecuteResult> {
    let idx_name_str = idx_name.to_string();
    let tbl_name = table_name;

    let mut schema = store
        .get_schema(txn, db_id, tbl_name)
        .await?
        .ok_or_else(|| SqlError::RelationNotFound(tbl_name.to_string()))?;

    // Schema-wide namespace uniqueness check (tables, views, matviews,
    // sequences, indexes, PK constraints). Also reserves the name via a
    // transactional KV key for concurrency safety.
    let owning_schema = tbl_name.split('.').next().unwrap_or("public");
    if !check_relation_name_available(
        store,
        txn,
        db_id,
        owning_schema,
        &idx_name_str,
        if_not_exists,
        None,
    )
    .await?
    {
        return Ok(ExecuteResult::CreateIndex {
            index_name: idx_name_str,
        });
    }

    let method = using.map(|m| m.value.to_lowercase());

    if let Some(pred_expr) = predicate {
        index_helpers::validate_index_predicate(pred_expr, &schema)?;
    }

    let predicate_str = predicate.map(|p| p.to_string());

    let mut idx_cols = Vec::new();
    let mut idx_exprs = Vec::new();
    for col_expr in columns {
        let mut expr = &col_expr.expr;
        while let Expr::Nested(inner) = expr {
            expr = inner.as_ref();
        }

        match expr {
            Expr::Identifier(ident) => {
                let col_name = normalize_ident(ident);
                if schema.column_index(&col_name).is_none() {
                    return Err(anyhow!("Column not found"));
                }
                idx_cols.push(col_name);
            }
            Expr::CompoundIdentifier(parts) => {
                let Some(last) = parts.last() else {
                    return Err(anyhow!("Index column must be identifier"));
                };
                let col_name = normalize_ident(last);
                if schema.column_index(&col_name).is_none() {
                    return Err(anyhow!("Column not found"));
                }
                idx_cols.push(col_name);
            }
            _ => {
                idx_exprs.push(expr.to_string());
            }
        }
    }

    // ── Operator class validation for GIN / GIST ────────────────────
    // PostgreSQL requires a default operator class for the index method.
    // For GIN: only array, jsonb, tsvector have defaults.
    // For GIST: only geometric/range/tsvector/tsquery types have defaults
    //           (most of which we don't support yet).
    if let Some(ref m) = method {
        let needs_opclass_check = matches!(m.as_str(), "gin" | "gist");
        if needs_opclass_check {
            for col_name in &idx_cols {
                if let Some(idx) = schema.column_index(col_name) {
                    let dt = &schema.columns[idx].data_type;
                    let has_default_opclass = match m.as_str() {
                        "gin" => matches!(
                            dt,
                            DataType::Array(_) | DataType::Jsonb | DataType::Tsvector
                        ),
                        "gist" => matches!(dt, DataType::Tsvector),
                        _ => true,
                    };
                    if !has_default_opclass {
                        return Err(anyhow!(
                            "data type {} has no default operator class for access method \"{}\"\nHINT:  You must specify an operator class for the index or define a default operator class for the data type.",
                            dt.to_string().to_lowercase(),
                            m
                        ));
                    }
                }
            }
            // Expression indexes on gin/gist: we can't easily infer the result
            // type of arbitrary expressions, so reject them unless they're on
            // known-good types (conservative approach matching PG behavior).
            if !idx_exprs.is_empty() && idx_cols.is_empty() {
                // For expression-only indexes we can't determine the type,
                // so let them through (PG would also accept if the expression
                // returns a type with a default opclass).
            }
        }
    }

    let index_id = schema
        .indexes
        .iter()
        .map(|i| i.id)
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| anyhow!("Index id overflow"))?;
    let new_index = IndexDef {
        name: idx_name_str.clone(),
        id: index_id,
        columns: idx_cols,
        unique,
        is_constraint: false,
        method,
        predicate: predicate_str,
        expressions: idx_exprs,
        state: if concurrently {
            IndexState::Building
        } else {
            IndexState::Ready
        },
    };

    if concurrently {
        schema.indexes.push(new_index);
        // Bump schema version so plan-cache drift detection catches index changes.
        schema.version += 1;
        store.update_schema(txn, db_id, schema.clone()).await?;

        if let Some(system_store) = crate::worker::get_system_store() {
            let entry = TaskQueueEntry::new(
                keyspace.to_string(),
                db_id,
                index_id as i64,
                TaskType::BgDdl,
                format!("__backfill_index {} {}", tbl_name, idx_name_str),
                username.to_string(),
                128,
            );
            let now_ms = chrono::Utc::now().timestamp_millis();
            let mut sys_txn = system_store.begin().await?;
            system_store
                .put_worker_queue_entry(&mut sys_txn, &entry, now_ms)
                .await?;
            system_store
                .update_registry_task_types(&mut sys_txn, keyspace, db_id, TASK_TYPE_BG_DDL, 0)
                .await?;
            sys_txn.commit().await?;
            // Wake the worker immediately so CIC does not wait for the poll interval.
            crate::worker::wake_worker();
        }

        return Ok(ExecuteResult::CreateIndex {
            index_name: idx_name_str,
        });
    }

    let mut current_batch_writes = 0usize;
    let mut has_committed_batches = false;

    let create_result: Result<()> = async {
        if index_helpers::is_index_materializable(&new_index) {
            if !rows.is_empty() {
                if schema.pk_indices.is_empty() {
                    let pk_types: Vec<DataType> = vec![DataType::Uuid];
                    let (start, end) =
                        crate::storage::encode_table_data_range_v2(db_id, schema.table_id);
                    let data_key_prefix = start.clone();
                    let mut scanner = KvScanBatches::new(start, end, DDL_SCAN_BATCH_SIZE);
                    while let Some(batch) = scanner.next_batch(txn).await? {
                        for pair in batch {
                            let key: &[u8] = pair.key().as_ref().into();
                            let pk_bytes = key
                                .strip_prefix(data_key_prefix.as_slice())
                                .ok_or_else(|| {
                                    anyhow!(
                                        "corrupted row key while backfilling index '{}'",
                                        idx_name_str
                                    )
                                })?;
                            let pk_values =
                                crate::storage::decode_pk_from_index_suffix(pk_bytes, &pk_types)?;

                            let mut row = crate::storage::deserialize_row(pair.value())?;
                            fill_row_defaults(&mut row, &schema)?;

                            if !index_helpers::eval_index_predicate(&new_index, &schema, &row)? {
                                continue;
                            }
                            let idx_values = index_helpers::get_index_values_with_expressions(
                                &new_index, &schema, &row,
                            )?;
                            store
                                .create_index_entry(
                                    txn,
                                    db_id,
                                    schema.table_id,
                                    index_id,
                                    &idx_values,
                                    &pk_values,
                                    unique,
                                )
                                .await?;
                            current_batch_writes += 1;
                            maybe_rotate_backfill_txn(
                                store,
                                txn,
                                &mut current_batch_writes,
                                &mut has_committed_batches,
                            )
                            .await?;
                        }
                    }
                } else {
                    for row in rows {
                        if !index_helpers::eval_index_predicate(&new_index, &schema, &row)? {
                            continue;
                        }
                        let idx_values = index_helpers::get_index_values_with_expressions(
                            &new_index, &schema, &row,
                        )?;
                        let pk_values = schema.get_pk_values(&row);
                        store
                            .create_index_entry(
                                txn,
                                db_id,
                                schema.table_id,
                                index_id,
                                &idx_values,
                                &pk_values,
                                unique,
                            )
                            .await?;
                        current_batch_writes += 1;
                        maybe_rotate_backfill_txn(
                            store,
                            txn,
                            &mut current_batch_writes,
                            &mut has_committed_batches,
                        )
                        .await?;
                    }
                }
            }
        } else if supported_gin_index_column(&schema, &new_index).is_some() && !rows.is_empty() {
            if schema.pk_indices.is_empty() {
                let pk_types: Vec<DataType> = vec![DataType::Uuid];
                let (start, end) =
                    crate::storage::encode_table_data_range_v2(db_id, schema.table_id);
                let data_key_prefix = start.clone();
                let mut scanner = KvScanBatches::new(start, end, DDL_SCAN_BATCH_SIZE);
                while let Some(batch) = scanner.next_batch(txn).await? {
                    for pair in batch {
                        let key: &[u8] = pair.key().as_ref().into();
                        let pk_bytes =
                            key.strip_prefix(data_key_prefix.as_slice())
                                .ok_or_else(|| {
                                    anyhow!(
                                        "corrupted row key while backfilling index '{}'",
                                        idx_name_str
                                    )
                                })?;
                        let pk_values =
                            crate::storage::decode_pk_from_index_suffix(pk_bytes, &pk_types)?;

                        let mut row = crate::storage::deserialize_row(pair.value())?;
                        fill_row_defaults(&mut row, &schema)?;

                        let hashes = extract_gin_token_hashes_from_row(&schema, &new_index, &row)?;
                        if hashes.is_empty() {
                            continue;
                        }
                        store
                            .create_gin_index_entries(
                                txn,
                                db_id,
                                schema.table_id,
                                index_id,
                                &hashes,
                                &pk_values,
                            )
                            .await?;
                        current_batch_writes += 1;
                        maybe_rotate_backfill_txn(
                            store,
                            txn,
                            &mut current_batch_writes,
                            &mut has_committed_batches,
                        )
                        .await?;
                    }
                }
            } else {
                for row in rows {
                    let hashes = extract_gin_token_hashes_from_row(&schema, &new_index, &row)?;
                    if hashes.is_empty() {
                        continue;
                    }
                    let pk_values = schema.get_pk_values(&row);
                    store
                        .create_gin_index_entries(
                            txn,
                            db_id,
                            schema.table_id,
                            index_id,
                            &hashes,
                            &pk_values,
                        )
                        .await?;
                    current_batch_writes += 1;
                    maybe_rotate_backfill_txn(
                        store,
                        txn,
                        &mut current_batch_writes,
                        &mut has_committed_batches,
                    )
                    .await?;
                }
            }
        }

        schema.indexes.push(new_index.clone());
        // Bump schema version so plan-cache drift detection catches index changes.
        schema.version += 1;
        store.update_schema(txn, db_id, schema.clone()).await?;
        Ok(())
    }
    .await;

    if let Err(err) = create_result {
        if has_committed_batches {
            // Backfill commits can succeed before schema update. On failure after that point,
            // remove committed entries so CREATE INDEX does not leave orphaned index KV data.
            // Also release the reservation key to prevent permanent false 42P07.
            let _ = txn.rollback().await;
            let (start, end) = index_prefix_range(db_id, schema.table_id, index_id);
            let idx_full_name = format!("{}.{}", owning_schema, idx_name_str);
            let cleanup_result: Result<()> = async {
                let mut cleanup_txn = store.begin().await?;
                delete_range(&mut cleanup_txn, start, end).await?;
                store
                    .release_relation_name(&mut cleanup_txn, db_id, &idx_full_name)
                    .await?;
                cleanup_txn.commit().await?;
                Ok(())
            }
            .await;

            *txn = store.begin().await?;

            if let Err(cleanup_err) = cleanup_result {
                return Err(err.context(format!(
                    "failed to cleanup partially backfilled index '{}': {}",
                    idx_name_str, cleanup_err
                )));
            }
        }
        return Err(err);
    }

    Ok(ExecuteResult::CreateIndex {
        index_name: idx_name_str,
    })
}

pub async fn update_index_state(
    store: &Arc<TikvStore>,
    db_id: u64,
    table_name: &str,
    index_name: &str,
    state: IndexState,
) -> Result<()> {
    let mut txn = store.begin().await?;
    let result: Result<()> = async {
        let mut schema = store
            .get_schema(&mut txn, db_id, table_name)
            .await?
            .ok_or_else(|| SqlError::RelationNotFound(table_name.to_string()))?;
        let idx = schema
            .indexes
            .iter_mut()
            .find(|idx| idx.name == index_name)
            .ok_or_else(|| anyhow!("Index '{}' not found on table '{}'", index_name, table_name))?;
        idx.state = state;
        store.update_schema(&mut txn, db_id, schema).await?;
        txn.commit().await?;
        Ok(())
    }
    .await;

    if let Err(e) = result {
        let _ = txn.rollback().await;
        return Err(e);
    }

    Ok(())
}

pub async fn backfill_index_by_name(
    store: &Arc<TikvStore>,
    db_id: u64,
    table_name: &str,
    index_name: &str,
    set_state_on_commit: Option<IndexState>,
) -> Result<()> {
    let mut txn = store.begin().await?;
    let mut current_batch_writes = 0usize;
    let mut has_committed_batches = false;

    let result: Result<()> = async {
        let schema = store
            .get_schema(&mut txn, db_id, table_name)
            .await?
            .ok_or_else(|| SqlError::RelationNotFound(table_name.to_string()))?;
        let index = schema
            .indexes
            .iter()
            .find(|idx| idx.name == index_name)
            .cloned()
            .ok_or_else(|| anyhow!("Index '{}' not found on table '{}'", index_name, table_name))?;

        let (start, end) = crate::storage::encode_table_data_range_v2(db_id, schema.table_id);
        let data_key_prefix = start.clone();
        let pk_types = pk_types_for_schema(&schema);

        if index_helpers::is_index_materializable(&index) {
            let mut scanner = KvScanBatches::new(start, end, DDL_SCAN_BATCH_SIZE);
            while let Some(batch) = scanner.next_batch(&mut txn).await? {
                for pair in batch {
                    let key: &[u8] = pair.key().as_ref().into();
                    let pk_values = if schema.pk_indices.is_empty() {
                        let pk_bytes =
                            key.strip_prefix(data_key_prefix.as_slice())
                                .ok_or_else(|| {
                                    anyhow!(
                                        "corrupted row key while backfilling index '{}'",
                                        index_name
                                    )
                                })?;
                        crate::storage::decode_pk_from_index_suffix(pk_bytes, &pk_types)?
                    } else {
                        let mut row = crate::storage::deserialize_row(pair.value())?;
                        fill_row_defaults(&mut row, &schema)?;
                        schema.get_pk_values(&row)
                    };

                    let mut row = crate::storage::deserialize_row(pair.value())?;
                    fill_row_defaults(&mut row, &schema)?;
                    if !index_helpers::eval_index_predicate(&index, &schema, &row)? {
                        continue;
                    }
                    let idx_values =
                        index_helpers::get_index_values_with_expressions(&index, &schema, &row)?;
                    let insert_result = store
                        .create_index_entry(
                            &mut txn,
                            db_id,
                            schema.table_id,
                            index.id,
                            &idx_values,
                            &pk_values,
                            index.unique,
                        )
                        .await;
                    if let Err(e) = insert_result {
                        if index.unique && is_unique_duplicate_error(&e) {
                            match resolve_unique_index_conflict(
                                store,
                                &mut txn,
                                db_id,
                                &schema,
                                &index,
                                &idx_values,
                                &pk_values,
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
                    current_batch_writes += 1;
                    maybe_rotate_backfill_txn(
                        store,
                        &mut txn,
                        &mut current_batch_writes,
                        &mut has_committed_batches,
                    )
                    .await?;
                }
            }
        } else if supported_gin_index_column(&schema, &index).is_some() {
            let mut scanner = KvScanBatches::new(start, end, DDL_SCAN_BATCH_SIZE);
            while let Some(batch) = scanner.next_batch(&mut txn).await? {
                for pair in batch {
                    let key: &[u8] = pair.key().as_ref().into();
                    let pk_values = if schema.pk_indices.is_empty() {
                        let pk_bytes =
                            key.strip_prefix(data_key_prefix.as_slice())
                                .ok_or_else(|| {
                                    anyhow!(
                                        "corrupted row key while backfilling index '{}'",
                                        index_name
                                    )
                                })?;
                        crate::storage::decode_pk_from_index_suffix(pk_bytes, &pk_types)?
                    } else {
                        let mut row = crate::storage::deserialize_row(pair.value())?;
                        fill_row_defaults(&mut row, &schema)?;
                        schema.get_pk_values(&row)
                    };

                    let mut row = crate::storage::deserialize_row(pair.value())?;
                    fill_row_defaults(&mut row, &schema)?;
                    let hashes = extract_gin_token_hashes_from_row(&schema, &index, &row)?;
                    if hashes.is_empty() {
                        continue;
                    }
                    store
                        .create_gin_index_entries(
                            &mut txn,
                            db_id,
                            schema.table_id,
                            index.id,
                            &hashes,
                            &pk_values,
                        )
                        .await?;
                    current_batch_writes += 1;
                    maybe_rotate_backfill_txn(
                        store,
                        &mut txn,
                        &mut current_batch_writes,
                        &mut has_committed_batches,
                    )
                    .await?;
                }
            }
        }

        if let Some(state) = set_state_on_commit {
            let mut schema = store
                .get_schema(&mut txn, db_id, table_name)
                .await?
                .ok_or_else(|| SqlError::RelationNotFound(table_name.to_string()))?;
            let idx = schema
                .indexes
                .iter_mut()
                .find(|idx| idx.name == index_name)
                .ok_or_else(|| {
                    anyhow!("Index '{}' not found on table '{}'", index_name, table_name)
                })?;
            idx.state = state;
            store.update_schema(&mut txn, db_id, schema).await?;
        }

        txn.commit().await?;
        Ok(())
    }
    .await;

    if let Err(e) = result {
        let _ = txn.rollback().await;
        return Err(e);
    }

    Ok(())
}

fn reconcile_index_search_path(table_name: &str) -> Vec<String> {
    let schema_name = table_name.split('.').next().unwrap_or("public");
    if schema_name.eq_ignore_ascii_case("public") {
        vec!["public".to_string(), "pg_catalog".to_string()]
    } else {
        vec![
            schema_name.to_string(),
            "public".to_string(),
            "pg_catalog".to_string(),
        ]
    }
}

fn infer_index_value_types_for_reconcile(
    index: &IndexDef,
    schema: &TableSchema,
    db_id: u64,
    table_name: &str,
    collations: &[crate::sql::collation::CollationDef],
) -> Result<Vec<DataType>> {
    let mut types = Vec::with_capacity(index.columns.len() + index.expressions.len());

    for col_name in &index.columns {
        let col = schema
            .columns
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(col_name))
            .ok_or_else(|| {
                anyhow!(
                    "Index column '{}' not found while reconciling '{}'",
                    col_name,
                    index.name
                )
            })?;
        types.push(col.data_type.clone());
    }

    let search_path = reconcile_index_search_path(table_name);
    for expr_str in &index.expressions {
        let dialect = sqlparser::dialect::PostgreSqlDialect {};
        let expr = sqlparser::parser::Parser::new(&dialect)
            .try_with_sql(expr_str)
            .and_then(|mut p| p.parse_expr())
            .map_err(|e| {
                anyhow!(
                    "failed to parse index expression '{}' on '{}': {}",
                    expr_str,
                    index.name,
                    e
                )
            })?;
        let typed = analyze_row_level_expr(&expr, schema, db_id, &search_path, collations)?;
        types.push(typed.data_type.clone());
    }

    Ok(types)
}

async fn reconcile_index_pass(
    store: &Arc<TikvStore>,
    db_id: u64,
    table_name: &str,
    index_name: &str,
    allow_rotate: bool,
    set_state_on_commit: Option<IndexState>,
) -> Result<()> {
    let mut txn = store.begin().await?;
    let mut current_batch_writes = 0usize;
    let mut has_committed_batches = false;

    let result: Result<()> = async {
        let schema = store
            .get_schema(&mut txn, db_id, table_name)
            .await?
            .ok_or_else(|| SqlError::RelationNotFound(table_name.to_string()))?;
        let index = schema
            .indexes
            .iter()
            .find(|idx| idx.name == index_name)
            .cloned()
            .ok_or_else(|| anyhow!("Index '{}' not found on table '{}'", index_name, table_name))?;

        if !index_helpers::is_index_materializable(&index) {
            if let Some(state) = set_state_on_commit {
                let mut schema = store
                    .get_schema(&mut txn, db_id, table_name)
                    .await?
                    .ok_or_else(|| SqlError::RelationNotFound(table_name.to_string()))?;
                let idx = schema
                    .indexes
                    .iter_mut()
                    .find(|idx| idx.name == index_name)
                    .ok_or_else(|| {
                        anyhow!("Index '{}' not found on table '{}'", index_name, table_name)
                    })?;
                idx.state = state;
                store.update_schema(&mut txn, db_id, schema).await?;
            }
            txn.commit().await?;
            return Ok(());
        }

        let pk_types = pk_types_for_schema(&schema);
        let collations = store.list_collations(&mut txn, db_id).await?;
        let index_value_types =
            infer_index_value_types_for_reconcile(&index, &schema, db_id, table_name, &collations)?;
        let (start, end) = index_prefix_range(db_id, schema.table_id, index.id);
        let mut scanner = KvScanBatches::new(start, end, DDL_SCAN_BATCH_SIZE);
        while let Some(batch) = scanner.next_batch(&mut txn).await? {
            for pair in batch {
                let scanned_key: Vec<u8> = {
                    let key: &[u8] = pair.key().as_ref().into();
                    key.to_vec()
                };
                let pk_values = if index.unique {
                    crate::storage::decode_pk_from_index_suffix(pair.value().as_ref(), &pk_types)?
                } else {
                    store.decode_non_unique_pk_from_index_key(
                        &scanned_key,
                        db_id,
                        schema.table_id,
                        index.id,
                        &index_value_types,
                        &pk_types,
                    )?
                };
                let existing_rows = store
                    .batch_get_rows(
                        &mut txn,
                        db_id,
                        schema.table_id,
                        vec![pk_values.clone()],
                        &schema,
                    )
                    .await?;

                let stale = if let Some(mut row) = existing_rows.into_iter().next() {
                    fill_row_defaults(&mut row, &schema)?;
                    if !index_helpers::eval_index_predicate(&index, &schema, &row)? {
                        true
                    } else {
                        let current_values = index_helpers::get_index_values_with_expressions(
                            &index, &schema, &row,
                        )?;
                        let expected_key = store.make_index_key(
                            db_id,
                            schema.table_id,
                            index.id,
                            &current_values,
                            if index.unique {
                                None
                            } else {
                                Some(pk_values.as_slice())
                            },
                        );
                        scanned_key != expected_key
                    }
                } else {
                    true
                };

                if stale {
                    txn_delete(&mut txn, scanned_key).await?;
                    current_batch_writes += 1;
                    if allow_rotate {
                        maybe_rotate_backfill_txn(
                            store,
                            &mut txn,
                            &mut current_batch_writes,
                            &mut has_committed_batches,
                        )
                        .await?;
                    }
                }
            }
        }

        if let Some(state) = set_state_on_commit {
            let mut schema = store
                .get_schema(&mut txn, db_id, table_name)
                .await?
                .ok_or_else(|| SqlError::RelationNotFound(table_name.to_string()))?;
            let idx = schema
                .indexes
                .iter_mut()
                .find(|idx| idx.name == index_name)
                .ok_or_else(|| {
                    anyhow!("Index '{}' not found on table '{}'", index_name, table_name)
                })?;
            idx.state = state;
            store.update_schema(&mut txn, db_id, schema).await?;
        }

        txn.commit().await?;
        Ok(())
    }
    .await;

    if let Err(e) = result {
        let _ = txn.rollback().await;
        return Err(e);
    }

    Ok(())
}

pub async fn reconcile_index(
    store: &Arc<TikvStore>,
    db_id: u64,
    table_name: &str,
    index_name: &str,
    set_state_on_commit: Option<IndexState>,
) -> Result<()> {
    // Pass 1 may commit partial cleanup batches via transaction rotation. This is safe:
    // Pass 2 always re-scans the full index range and is the authoritative verification
    // pass before any Ready state transition is committed.
    reconcile_index_pass(store, db_id, table_name, index_name, true, None).await?;
    // Pass 2: short final verification + optional atomic state flip.
    reconcile_index_pass(
        store,
        db_id,
        table_name,
        index_name,
        false,
        set_state_on_commit,
    )
    .await
}
