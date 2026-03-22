//! CREATE INDEX, index backfill, index state management, and index
//! reconciliation for CREATE INDEX CONCURRENTLY.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use sqlparser::ast::{Expr, Ident, OrderByExpr};
use tikv_client::Transaction;
use usearch::ffi::{IndexOptions, ScalarKind};

use crate::model::{build_predicate_conjunct_cache, DataType, IndexDef, Row, TableSchema, Value};
use crate::sql::error::SqlError;
use crate::sql::gin::{extract_gin_token_hashes_from_row, supported_gin_index_column};
use crate::sql::hnsw::storage::{hnsw_graph_key, hnsw_meta_key, serialize_hnsw_snapshot};
use crate::sql::hnsw::{
    metric_from_string, vec_f64_to_f32, HnswMeta, HNSW_DEFAULT_EF_CONSTRUCTION,
    HNSW_DEFAULT_EF_SEARCH, HNSW_DEFAULT_M,
};
use crate::sql::index_consistency::{
    is_unique_duplicate_error, pk_types_for_schema, resolve_unique_index_conflict,
    UniqueConflictResolution,
};
use crate::sql::index_helpers;
use crate::sql::names::normalize_ident;
use crate::sql::projection::fill_row_defaults;
use crate::sql::ExecuteResult;
use crate::storage::TikvStore;
use crate::txn::{txn_delete, txn_put};
use crate::worker::types::{IndexState, TaskQueueEntry, TaskType, TASK_TYPE_BG_DDL};

use super::create_table::{check_relation_name_available, RelationKind};
use super::{
    analyze_row_level_expr, delete_range, index_prefix_range, maybe_rotate_backfill_txn,
    KvScanBatches, DDL_SCAN_BATCH_SIZE,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HnswMethodVariant {
    L2Default,
    L2Explicit,
    Cosine,
    Ip,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedIndexMethod {
    /// Access method persisted in schema metadata (`None` => default btree).
    storage_method: Option<String>,
    /// HNSW metric encoded by parser preprocessor suffix.
    hnsw_variant: Option<HnswMethodVariant>,
}

impl ResolvedIndexMethod {
    fn is_hnsw(&self) -> bool {
        self.hnsw_variant.is_some()
    }
}

fn hnsw_opclass_name(variant: HnswMethodVariant) -> Option<&'static str> {
    match variant {
        HnswMethodVariant::L2Default => None,
        HnswMethodVariant::L2Explicit => Some("vector_l2_ops"),
        HnswMethodVariant::Cosine => Some("vector_cosine_ops"),
        HnswMethodVariant::Ip => Some("vector_ip_ops"),
    }
}

fn resolve_create_index_method(using: Option<&Ident>) -> Result<ResolvedIndexMethod> {
    let Some(method_ident) = using else {
        return Ok(ResolvedIndexMethod {
            storage_method: None,
            hnsw_variant: None,
        });
    };

    let method_raw = method_ident.value.to_ascii_lowercase();
    let resolved = match method_raw.as_str() {
        "hnsw" => ResolvedIndexMethod {
            storage_method: Some("hnsw".to_string()),
            hnsw_variant: Some(HnswMethodVariant::L2Default),
        },
        "hnsw__l2" => ResolvedIndexMethod {
            storage_method: Some("hnsw".to_string()),
            hnsw_variant: Some(HnswMethodVariant::L2Explicit),
        },
        "hnsw__cosine" => ResolvedIndexMethod {
            storage_method: Some("hnsw".to_string()),
            hnsw_variant: Some(HnswMethodVariant::Cosine),
        },
        "hnsw__ip" => ResolvedIndexMethod {
            storage_method: Some("hnsw".to_string()),
            hnsw_variant: Some(HnswMethodVariant::Ip),
        },
        "hash" | "gist" | "spgist" | "brin" => {
            return Err(anyhow!(
                "access method \"{}\" is not supported\nHINT: Only btree, gin, and hnsw indexes are currently supported.",
                method_raw
            ));
        }
        "btree" | "gin" => ResolvedIndexMethod {
            storage_method: Some(method_raw),
            hnsw_variant: None,
        },
        _ => {
            return Err(anyhow!(
                "access method \"{}\" does not exist",
                method_ident.value
            ));
        }
    };

    Ok(resolved)
}

#[allow(clippy::too_many_arguments)]
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
    with_params: Option<&str>,
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
        RelationKind::Index,
        if_not_exists,
        None,
    )
    .await?
    {
        return Ok(ExecuteResult::CreateIndex {
            index_name: idx_name_str,
        });
    }

    let resolved_method = resolve_create_index_method(using)?;
    let is_hnsw = resolved_method.is_hnsw();
    let method = resolved_method.storage_method.clone();
    let storage_params = parse_index_storage_params(with_params)?;

    if !is_hnsw {
        validate_non_hnsw_build_params(&storage_params)?;
    }

    if is_hnsw && concurrently {
        return Err(anyhow!(
            "CREATE INDEX CONCURRENTLY is not supported for HNSW indexes"
        ));
    }

    if is_hnsw && predicate.is_some() {
        return Err(anyhow!(
            "HNSW indexes do not support partial index predicates (WHERE clause)"
        ));
    }

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

    let mut hnsw_m: Option<u16> = None;
    let mut hnsw_ef_construction: Option<u16> = None;
    let mut hnsw_distance_metric: Option<String> = None;
    if is_hnsw {
        if unique {
            return Err(SqlError::Unsupported(
                "access method \"hnsw\" does not support unique indexes".into(),
            )
            .into());
        }
        if idx_cols.len() != 1 || !idx_exprs.is_empty() {
            return Err(anyhow!(
                "access method \"hnsw\" does not support multicolumn indexes"
            ));
        }

        // HNSW requires a single-column primary key.
        // Integer PKs use Direct mode (label = PK), others use Mapped mode
        // (label = internal rowid with persistent bidirectional mapping).
        if schema.pk_indices.len() != 1 {
            return Err(anyhow!("HNSW indexes require a single-column primary key"));
        }
        let indexed_col = idx_cols[0].clone();
        let col_idx = schema
            .column_index(&indexed_col)
            .ok_or_else(|| anyhow!("Column not found"))?;
        let col_data_type = &schema.columns[col_idx].data_type;
        if !matches!(col_data_type, DataType::Vector(_)) {
            if let Some(variant) = resolved_method.hnsw_variant {
                if let Some(opclass) = hnsw_opclass_name(variant) {
                    return Err(anyhow!(
                        "operator class \"{}\" does not accept data type {}",
                        opclass,
                        col_data_type.to_string().to_lowercase()
                    ));
                }
            }
            return Err(anyhow!(
                "data type {} has no default operator class for access method \"hnsw\"\nHINT:  You must specify an operator class for the index or define a default operator class for the data type.",
                col_data_type.to_string().to_lowercase()
            ));
        }

        // Distance metric is sourced from the preprocessor suffix when present
        // (`hnsw__l2` / `hnsw__cosine` / `hnsw__ip`). For plain `USING hnsw`,
        // allow opclass parsing as a compatibility fallback for direct API
        // callers.
        let metric = match resolved_method.hnsw_variant {
            Some(HnswMethodVariant::L2Explicit) => "l2".to_string(),
            Some(HnswMethodVariant::Cosine) => "cosine".to_string(),
            Some(HnswMethodVariant::Ip) => "ip".to_string(),
            Some(HnswMethodVariant::L2Default) => {
                let opclass = parse_hnsw_operator_class(columns, &indexed_col)?;
                parse_hnsw_distance_metric(opclass.as_deref())?
            }
            None => return Err(anyhow!("internal error: HNSW index without HNSW method")),
        };
        let (m, ef_construction) = parse_hnsw_build_params(&storage_params)?;

        hnsw_m = Some(m as u16);
        hnsw_ef_construction = Some(ef_construction as u16);
        hnsw_distance_metric = Some(metric);
    }

    // ── Operator class validation for GIN ─────────────────────────────
    // PostgreSQL requires a default operator class for the index method.
    // For GIN: only array, jsonb, tsvector have defaults.
    if let Some(ref m) = method {
        let needs_opclass_check = matches!(m.as_str(), "gin");
        if needs_opclass_check {
            for col_name in &idx_cols {
                if let Some(idx) = schema.column_index(col_name) {
                    let dt = &schema.columns[idx].data_type;
                    let has_default_opclass = matches!(
                        dt,
                        DataType::Array(_) | DataType::Jsonb | DataType::Tsvector
                    );
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
    let mut new_index = IndexDef {
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
        cached_predicate_conjuncts: None,
        hnsw_m,
        hnsw_ef_construction,
        hnsw_distance_metric,
    };
    new_index.cached_predicate_conjuncts =
        build_predicate_conjunct_cache(new_index.predicate.as_deref());

    if concurrently {
        // CONCURRENTLY requires the worker to process background DDL.
        // Without it, the index stays in Building state forever.
        require_worker_for_index(
            "CONCURRENTLY",
            &idx_name_str,
            "Background index builds require the worker engine. \
             Enable the worker or use CREATE INDEX (without CONCURRENTLY).",
        )?;

        schema.indexes.push(new_index);
        // Bump schema version so plan-cache drift detection catches index changes.
        schema.version += 1;
        store.update_schema(txn, db_id, schema.clone()).await?;

        {
            let system_store = crate::worker::get_system_store()
                .expect("require_worker_for_index guarantees Some");
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
        } // end system_store block

        return Ok(ExecuteResult::CreateIndex {
            index_name: idx_name_str,
        });
    }

    let mut current_batch_writes = 0usize;
    let mut has_committed_batches = false;

    let create_result: Result<()> = async {
        if new_index.is_hnsw() {
            // HNSW indexes require the worker subsystem for background delta-log
            // merge. Reject early — before expensive table scan + index build.
            require_worker_for_index(
                "HNSW",
                &idx_name_str,
                "HNSW indexes require background merge via the worker engine. \
                 Enable the worker or use a btree index.",
            )?;

            let col_name = new_index
                .columns
                .first()
                .ok_or_else(|| anyhow!("HNSW indexes only support single vector columns"))?
                .clone();
            let col_idx = schema
                .column_index(&col_name)
                .ok_or_else(|| anyhow!("Column not found"))?;
            let vector_dimensions = match schema.columns[col_idx].data_type {
                DataType::Vector(dim) => dim as usize,
                _ => return Err(anyhow!("Column {} is not a vector type", col_name)),
            };

            let distance_metric = new_index
                .hnsw_distance_metric
                .clone()
                .unwrap_or_else(|| "l2".to_string());
            let metric = metric_from_string(distance_metric.as_str())
                .map_err(|e| anyhow!("failed to parse HNSW distance metric: {}", e))?;
            let m = usize::from(new_index.hnsw_m.unwrap_or(HNSW_DEFAULT_M as u16));
            let ef_construction = usize::from(
                new_index
                    .hnsw_ef_construction
                    .unwrap_or(HNSW_DEFAULT_EF_CONSTRUCTION as u16),
            );

            let (start, end) = crate::storage::encode_table_data_range_v2(db_id, schema.table_id);
            let data_key_prefix = start.clone();
            let pk_types = pk_types_for_schema(&schema);

            // Determine label mode from PK type.
            let pk_col_type = &schema.columns[schema.pk_indices[0]].data_type;
            let label_mode = if matches!(pk_col_type, DataType::Int32 | DataType::Int64) {
                crate::sql::hnsw::HnswLabelMode::Direct
            } else {
                crate::sql::hnsw::HnswLabelMode::Mapped
            };

            // ── Reject tables with negative PK values (Direct mode only) ──
            // In Direct mode, usearch labels are derived from the PK value and
            // must be non-negative u64. In Mapped mode, internal rowids are used
            // so negative PKs are fine.
            if label_mode == crate::sql::hnsw::HnswLabelMode::Direct {
                let range: tikv_client::BoundRange = (start.clone()..end.clone()).into();
                let pairs: Vec<tikv_client::KvPair> = txn.scan(range, 1).await?.collect();
                if let Some(pair) = pairs.first() {
                    let mut row = crate::storage::deserialize_row(pair.value())?;
                    fill_row_defaults(&mut row, &schema)?;
                    let pk_values = if schema.pk_indices.is_empty() {
                        let key: &[u8] = pair.key().as_ref().into();
                        let pk_bytes =
                            key.strip_prefix(data_key_prefix.as_slice())
                                .ok_or_else(|| {
                                    anyhow!("corrupted row key while validating HNSW index")
                                })?;
                        crate::storage::decode_pk_from_index_suffix(pk_bytes, &pk_types)?
                    } else {
                        schema.get_pk_values(&row)
                    };
                    let is_negative = pk_values.iter().any(|v| match v {
                        Value::Int32(n) => *n < 0,
                        Value::Int64(n) => *n < 0,
                        _ => false,
                    });
                    if is_negative {
                        let pk_display = pk_values
                            .iter()
                            .map(|v| v.to_string())
                            .collect::<Vec<_>>()
                            .join(", ");
                        return Err(anyhow!(
                            "cannot create HNSW index: primary key contains negative value \
                             ({}); HNSW indexes require non-negative INTEGER/BIGINT primary keys",
                            pk_display
                        ));
                    }
                }
            }

            let mut scanner = KvScanBatches::new(start, end, DDL_SCAN_BATCH_SIZE);
            let mut pending_vectors: Vec<(u64, Vec<f32>)> = Vec::new();
            let mut count = 0u64;
            while let Some(batch) = scanner.next_batch(txn).await? {
                for pair in batch {
                    let key: &[u8] = pair.key().as_ref().into();
                    let mut row = crate::storage::deserialize_row(pair.value())?;
                    fill_row_defaults(&mut row, &schema)?;

                    let pk_values = if schema.pk_indices.is_empty() {
                        let pk_bytes =
                            key.strip_prefix(data_key_prefix.as_slice())
                                .ok_or_else(|| {
                                    anyhow!(
                                        "corrupted row key while backfilling index '{}'",
                                        idx_name_str
                                    )
                                })?;
                        crate::storage::decode_pk_from_index_suffix(pk_bytes, &pk_types)?
                    } else {
                        schema.get_pk_values(&row)
                    };

                    let vector = match row.values.get(col_idx) {
                        Some(Value::Null) | None => continue,
                        Some(Value::Vector(v)) => v,
                        Some(_) => return Err(anyhow!("Column {} is not a vector type", col_name)),
                    };
                    let pk_label = crate::sql::hnsw::hnsw_resolve_label(
                        label_mode,
                        txn,
                        store,
                        db_id,
                        schema.table_id,
                        &pk_values,
                    )
                    .await?;
                    pending_vectors.push((pk_label, vec_f64_to_f32(vector)));
                    count = count.saturating_add(1);
                }
            }

            let (graph_bytes, meta_bytes) = {
                let options = IndexOptions {
                    dimensions: vector_dimensions,
                    metric,
                    quantization: ScalarKind::F32,
                    connectivity: m,
                    expansion_add: ef_construction,
                    expansion_search: HNSW_DEFAULT_EF_SEARCH,
                };
                let index = usearch::ffi::new_index(&options)
                    .map_err(|e| anyhow!("failed to create HNSW index: {}", e))?;
                // Reserve capacity before adding — usearch segfaults on add()
                // to an unreserved index (0 capacity from new_index).
                if !pending_vectors.is_empty() {
                    index
                        .reserve(pending_vectors.len())
                        .map_err(|e| anyhow!("failed to reserve HNSW capacity: {}", e))?;
                }
                for (pk_label, vector) in &pending_vectors {
                    index
                        .add(*pk_label, vector)
                        .map_err(|e| anyhow!("failed to add vector to HNSW index: {}", e))?;
                }

                let meta = HnswMeta {
                    count,
                    capacity: index.capacity() as u64,
                    dimensions: vector_dimensions,
                    distance_metric,
                    m,
                    ef_construction,
                    storage_version: 1, // New indexes use delta-log from the start
                    label_mode,
                    frozen: false,
                };
                serialize_hnsw_snapshot(db_id, schema.table_id, index_id, &index, &meta)
                    .map_err(|e| anyhow!("failed to serialize HNSW index: {}", e))?
            };

            // Register this (keyspace, db_id) in the worker registry so the periodic
            // HNSW sweeper can discover it. This is FATAL: if registration fails,
            // CREATE INDEX fails. This guarantees no HNSW index can exist without
            // a registry entry — closing the crash-orphan discovery gap completely.
            //
            // Safety: get_system_store() is guaranteed Some — we checked at the top
            // of the HNSW branch and returned an error if None.
            {
                let system_store = crate::worker::get_system_store()
                    .expect("worker check at HNSW branch entry guarantees Some");
                let mut sys_txn = system_store.begin().await?;
                system_store
                    .update_registry_task_types(
                        &mut sys_txn,
                        keyspace,
                        db_id,
                        crate::worker::types::TASK_TYPE_HNSW_MERGE,
                        0,
                    )
                    .await?;
                sys_txn.commit().await?;
            }

            // Guard: reject CREATE INDEX if the initial graph would exceed
            // the raft-entry-safe limit. This prevents the same oversized
            // monolithic write that the merge-path freeze guards against.
            if graph_bytes.len() > crate::worker::engine::HNSW_GRAPH_MAX_BYTES {
                return Err(anyhow!(
                    "HNSW index too large for initial build ({} bytes, limit {} bytes). \
                     Reduce table size or vector dimensions before creating the index.",
                    graph_bytes.len(),
                    crate::worker::engine::HNSW_GRAPH_MAX_BYTES
                ));
            }

            txn_put(
                txn,
                hnsw_graph_key(db_id, schema.table_id, index_id),
                graph_bytes,
            )
            .await?;
            txn_put(
                txn,
                hnsw_meta_key(db_id, schema.table_id, index_id),
                meta_bytes,
            )
            .await?;
        } else if index_helpers::is_index_materializable(&new_index) {
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
                                    new_index.unique,
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
                                new_index.unique,
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

/// Gate: reject index creation when worker subsystem is unavailable.
/// Used by both HNSW (needs background merge) and CONCURRENTLY (needs BgDdl).
fn require_worker_for_index(feature: &str, index_name: &str, reason: &str) -> Result<()> {
    if crate::worker::get_system_store().is_none() {
        return Err(anyhow!(
            "Cannot create {} index '{}': worker subsystem is disabled \
             (DB9_WORKER_ENABLED=false). {}",
            feature,
            index_name,
            reason
        ));
    }
    Ok(())
}

fn parse_hnsw_operator_class(columns: &[OrderByExpr], column_name: &str) -> Result<Option<String>> {
    if columns.is_empty() {
        return Ok(None);
    }

    let raw_expr = columns[0].expr.to_string();
    let compact = raw_expr.trim();
    let normalized_col = column_name.to_ascii_lowercase();

    if compact.eq_ignore_ascii_case(column_name) {
        return Ok(None);
    }

    let mut tokens = compact.split_whitespace();
    let first = tokens.next().unwrap_or_default().trim_matches('"');
    let first = first.to_ascii_lowercase();
    let second = tokens.next();
    let third = tokens.next();

    if first == normalized_col {
        if let (Some(opclass), None) = (second, third) {
            return Ok(Some(opclass.to_ascii_lowercase()));
        }
    }

    Ok(None)
}

fn parse_hnsw_distance_metric(opclass: Option<&str>) -> Result<String> {
    match opclass {
        None => Ok("l2".to_string()),
        Some("vector_l2_ops") => Ok("l2".to_string()),
        Some("vector_cosine_ops") => Ok("cosine".to_string()),
        Some("vector_ip_ops") => Ok("ip".to_string()),
        Some(other) => Err(anyhow!("Unknown operator class: {}", other)),
    }
}

type IndexStorageParam = (String, String);

fn parse_index_storage_params(with_params: Option<&str>) -> Result<Vec<IndexStorageParam>> {
    let Some(with_params) = with_params else {
        return Ok(Vec::new());
    };
    let trimmed = with_params.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    let mut params = Vec::new();
    for part in trimmed.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (key_raw, value_raw) = part
            .split_once('=')
            .ok_or_else(|| anyhow!("invalid storage parameter syntax: \"{}\"", part))?;
        let key = key_raw.trim().to_ascii_lowercase();
        let value = value_raw.trim().to_string();
        if key.is_empty() || value.is_empty() {
            return Err(anyhow!("invalid storage parameter syntax: \"{}\"", part));
        }
        params.push((key, value));
    }
    Ok(params)
}

fn validate_non_hnsw_build_params(params: &[IndexStorageParam]) -> Result<()> {
    if let Some((key, _)) = params.first() {
        return Err(anyhow!("unrecognized parameter \"{}\"", key));
    }
    Ok(())
}

fn parse_hnsw_build_params(params: &[IndexStorageParam]) -> Result<(usize, usize)> {
    let mut m = HNSW_DEFAULT_M;
    let mut ef_construction = HNSW_DEFAULT_EF_CONSTRUCTION;

    for (key, value) in params {
        match key.as_str() {
            "m" => {
                m = value
                    .parse::<usize>()
                    .map_err(|_| anyhow!("m must be between 2 and 100"))?;
            }
            "ef_construction" => {
                ef_construction = value
                    .parse::<usize>()
                    .map_err(|_| anyhow!("ef_construction must be between 4 and 1000"))?;
            }
            _ => {
                return Err(anyhow!("unrecognized parameter \"{}\"", key));
            }
        }
    }

    if !(2..=100).contains(&m) {
        return Err(anyhow!("m must be between 2 and 100"));
    }
    if !(4..=1000).contains(&ef_construction) {
        return Err(anyhow!("ef_construction must be between 4 and 1000"));
    }
    if ef_construction < 2 * m {
        return Err(anyhow!("ef_construction must be >= 2*m"));
    }

    Ok((m, ef_construction))
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
                    store.decode_unique_pk_from_index_entry(
                        &scanned_key,
                        pair.value().as_ref(),
                        db_id,
                        schema.table_id,
                        index.id,
                        &index_value_types,
                        &pk_types,
                    )?
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
                        let expected_pk_suffix = if !index.unique
                            || current_values
                                .iter()
                                .any(|value| matches!(value, Value::Null))
                        {
                            Some(pk_values.as_slice())
                        } else {
                            None
                        };
                        let expected_key = store.make_index_key(
                            db_id,
                            schema.table_id,
                            index.id,
                            &current_values,
                            expected_pk_suffix,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ColumnDef;

    fn test_schema() -> TableSchema {
        TableSchema::new(
            "custom.t".to_string(),
            1,
            vec![
                ColumnDef {
                    name: "id".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: true,
                    unique: true,
                    is_serial: false,
                    default_expr: None,
                    generation_expr: None,
                    generation_expr_authorized_by: None,
                    collation: None,
                },
                ColumnDef {
                    name: "name".to_string(),
                    data_type: DataType::Text,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    generation_expr: None,
                    generation_expr_authorized_by: None,
                    collation: None,
                },
            ],
            vec![0],
        )
    }

    fn test_index(columns: Vec<&str>, expressions: Vec<&str>) -> IndexDef {
        IndexDef {
            name: "idx_test".to_string(),
            id: 1,
            columns: columns.into_iter().map(ToString::to_string).collect(),
            unique: false,
            is_constraint: false,
            method: None,
            predicate: None,
            expressions: expressions.into_iter().map(ToString::to_string).collect(),
            state: IndexState::Ready,
            cached_predicate_conjuncts: None,
            hnsw_m: None,
            hnsw_ef_construction: None,
            hnsw_distance_metric: None,
        }
    }

    #[test]
    fn reconcile_index_search_path_prefers_public_then_pg_catalog() {
        let path = reconcile_index_search_path("public.t1");
        assert_eq!(path, vec!["public", "pg_catalog"]);
    }

    #[test]
    fn reconcile_index_search_path_keeps_non_public_schema_first() {
        let path = reconcile_index_search_path("tenant_a.t1");
        assert_eq!(path, vec!["tenant_a", "public", "pg_catalog"]);
    }

    #[test]
    fn infer_index_value_types_reads_columns_and_expressions() {
        let schema = test_schema();
        let index = test_index(vec!["id"], vec!["id"]);
        let types = infer_index_value_types_for_reconcile(&index, &schema, 1, "custom.t", &[])
            .expect("infer index value types");
        assert_eq!(types, vec![DataType::Int32, DataType::Int32]);
    }

    #[test]
    fn infer_index_value_types_errors_on_missing_column() {
        let schema = test_schema();
        let index = test_index(vec!["missing_col"], vec![]);
        let err = infer_index_value_types_for_reconcile(&index, &schema, 1, "custom.t", &[])
            .expect_err("missing column should error");
        assert!(
            err.to_string()
                .contains("Index column 'missing_col' not found"),
            "unexpected err: {err}"
        );
    }

    #[test]
    fn infer_index_value_types_errors_on_unparseable_expression() {
        let schema = test_schema();
        let index = test_index(vec![], vec!["("]);
        let err = infer_index_value_types_for_reconcile(&index, &schema, 1, "custom.t", &[])
            .expect_err("bad expression should error");
        assert!(
            err.to_string()
                .contains("failed to parse index expression '('"),
            "unexpected err: {err}"
        );
    }

    #[test]
    fn resolve_create_index_method_accepts_hnsw_suffixes() {
        let base = resolve_create_index_method(Some(&Ident::new("hnsw")))
            .expect("resolve hnsw base method");
        assert_eq!(base.storage_method.as_deref(), Some("hnsw"));
        assert_eq!(base.hnsw_variant, Some(HnswMethodVariant::L2Default));

        let l2 = resolve_create_index_method(Some(&Ident::new("hnsw__l2")))
            .expect("resolve hnsw l2 sentinel");
        assert_eq!(l2.storage_method.as_deref(), Some("hnsw"));
        assert_eq!(l2.hnsw_variant, Some(HnswMethodVariant::L2Explicit));

        let cosine = resolve_create_index_method(Some(&Ident::new("hnsw__cosine")))
            .expect("resolve hnsw cosine sentinel");
        assert_eq!(cosine.storage_method.as_deref(), Some("hnsw"));
        assert_eq!(cosine.hnsw_variant, Some(HnswMethodVariant::Cosine));

        let ip = resolve_create_index_method(Some(&Ident::new("hnsw__ip")))
            .expect("resolve hnsw ip sentinel");
        assert_eq!(ip.storage_method.as_deref(), Some("hnsw"));
        assert_eq!(ip.hnsw_variant, Some(HnswMethodVariant::Ip));
    }

    #[test]
    fn resolve_create_index_method_rejects_unknown_method() {
        let err = resolve_create_index_method(Some(&Ident::new("hnsw__evil")))
            .expect_err("unknown access method should be rejected");
        assert!(
            err.to_string()
                .contains("access method \"hnsw__evil\" does not exist"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_create_index_method_rejects_unsupported_methods() {
        for method in &["hash", "gist", "spgist", "brin"] {
            let err = resolve_create_index_method(Some(&Ident::new(*method)))
                .expect_err(&format!("{method} should be rejected"));
            assert!(
                err.to_string().contains("is not supported"),
                "unexpected error for {method}: {err}"
            );
        }
    }

    #[test]
    fn resolve_create_index_method_accepts_btree_and_gin() {
        for method in &["btree", "gin"] {
            let result = resolve_create_index_method(Some(&Ident::new(*method)))
                .unwrap_or_else(|e| panic!("{method} should be accepted: {e}"));
            assert_eq!(result.storage_method.as_deref(), Some(*method));
            assert_eq!(result.hnsw_variant, None);
        }
    }

    #[test]
    fn parse_index_storage_params_parses_key_values() {
        let params = parse_index_storage_params(Some("m = 32, ef_construction = 128"))
            .expect("parse storage params");
        assert_eq!(
            params,
            vec![
                ("m".to_string(), "32".to_string()),
                ("ef_construction".to_string(), "128".to_string())
            ]
        );
    }

    #[test]
    fn parse_hnsw_build_params_rejects_unknown_parameter() {
        let params = vec![("unknown_param".to_string(), "1".to_string())];
        let err = parse_hnsw_build_params(&params).expect_err("unknown hnsw param should fail");
        assert!(
            err.to_string()
                .contains("unrecognized parameter \"unknown_param\""),
            "unexpected error: {err}"
        );
    }

    /// Regression test: require_worker_for_index (used by both HNSW and
    /// CONCURRENTLY paths) must reject when worker system store is absent.
    /// In the test environment the global SYSTEM_STORE OnceLock is never set,
    /// so this exercises the actual production guard function.
    #[test]
    fn require_worker_for_index_rejects_when_worker_absent() {
        // Precondition: system store not initialized in test harness
        assert!(
            crate::worker::get_system_store().is_none(),
            "test expects worker system store to be unset"
        );

        let err = require_worker_for_index("HNSW", "idx_test", "reason")
            .expect_err("should reject when worker is absent");
        let msg = err.to_string();
        assert!(
            msg.contains("worker subsystem is disabled"),
            "error should mention worker disabled, got: {msg}"
        );
        assert!(
            msg.contains("idx_test"),
            "error should include index name, got: {msg}"
        );
        assert!(
            msg.contains("HNSW"),
            "error should include feature name, got: {msg}"
        );
    }

    /// Regression test: CONCURRENTLY path also requires worker.
    #[test]
    fn require_worker_for_index_rejects_concurrently_when_worker_absent() {
        assert!(crate::worker::get_system_store().is_none());

        let err = require_worker_for_index("CONCURRENTLY", "idx_cic", "needs BgDdl")
            .expect_err("CONCURRENTLY should reject without worker");
        let msg = err.to_string();
        assert!(
            msg.contains("CONCURRENTLY"),
            "error should include feature, got: {msg}"
        );
        assert!(
            msg.contains("idx_cic"),
            "error should include index name, got: {msg}"
        );
    }

    #[test]
    fn validate_non_hnsw_build_params_rejects_any_parameter() {
        let params = vec![("not_a_real_option".to_string(), "123".to_string())];
        let err = validate_non_hnsw_build_params(&params)
            .expect_err("non-hnsw params should not be silently accepted");
        assert!(
            err.to_string()
                .contains("unrecognized parameter \"not_a_real_option\""),
            "unexpected error: {err}"
        );
    }
}
