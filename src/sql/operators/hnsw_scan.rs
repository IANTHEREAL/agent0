use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

use tokio::sync::Semaphore;

use super::{ExecutionContext, PhysicalOperator};
use crate::model::{DataType, Row, TableSchema, Value};
use crate::sql::analyzer::types::TypedExpr;
use crate::sql::hnsw::s3::SharedHnswIndex;
use crate::sql::hnsw::storage::{
    batch_get_pk_for_rowids, build_delta_index, get_shared_base_graph, hnsw_meta_key,
    max_deltas_for_budget, merge_search_results, scan_visible_deltas, HnswMeta,
};
use crate::sql::hnsw::{vec_f64_to_f32, HnswDistanceMetric, HnswIndexHandle, HnswLabelMode};

/// Process-level semaphore bounding concurrent usearch search() calls.
///
/// usearch's internal thread context pool has `hardware_concurrency()` slots.
/// Calling search() with more concurrent callers than slots causes undefined
/// behavior (empty vector access in thread_lock_()). This semaphore prevents
/// that by limiting concurrent HNSW searches to `available_parallelism()`.
fn hnsw_search_semaphore() -> &'static Semaphore {
    static SEM: OnceLock<Semaphore> = OnceLock::new();
    SEM.get_or_init(|| {
        let permits = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(8);
        Semaphore::new(permits)
    })
}
use crate::sql::projection::fill_row_defaults;
use crate::storage::decode_pk_from_index_suffix;

#[allow(dead_code)] // fields used in explain_info() trait method
#[derive(Debug)]
pub struct HnswScanOperator {
    schema: TableSchema,
    index_id: u64,
    index_name: String,
    query_vector: Vec<Value>,
    k: usize,
    distance_metric: HnswDistanceMetric,
    distance_expr: Option<TypedExpr>,
    row_buffer: Vec<Row>,
    position: usize,
    opened: bool,
}

impl HnswScanOperator {
    pub fn new(
        schema: TableSchema,
        index_id: u64,
        index_name: String,
        query_vector: Vec<Value>,
        k: usize,
        distance_metric: HnswDistanceMetric,
        distance_expr: Option<TypedExpr>,
    ) -> Self {
        Self {
            schema,
            index_id,
            index_name,
            query_vector,
            k,
            distance_metric,
            distance_expr,
            row_buffer: Vec::new(),
            position: 0,
            opened: false,
        }
    }

    fn pk_as_u64(value: &Value) -> Option<u64> {
        match value {
            Value::Int32(v) => u64::try_from(*v).ok(),
            Value::Int64(v) => u64::try_from(*v).ok(),
            _ => None,
        }
    }

    fn pk_value_from_label(&self, label: u64) -> Result<Value> {
        let pk_col_idx = self.schema.pk_indices.first().copied().unwrap_or(0);
        let pk_ty = self
            .schema
            .columns
            .get(pk_col_idx)
            .map(|c| c.data_type.clone())
            .unwrap_or(DataType::Int64);

        match pk_ty {
            DataType::Int64 => {
                let v = i64::try_from(label)
                    .map_err(|_| anyhow!("HNSW label {} does not fit BIGINT primary key", label))?;
                Ok(Value::Int64(v))
            }
            DataType::Int32 => {
                let v = i32::try_from(label).map_err(|_| {
                    anyhow!("HNSW label {} does not fit INTEGER primary key", label)
                })?;
                Ok(Value::Int32(v))
            }
            other => Err(anyhow!(
                "HNSW scan currently supports INTEGER/BIGINT PK only, found {}",
                other
            )),
        }
    }

    fn search_and_rank(
        index: &HnswIndexHandle,
        query_f32: &[f32],
        k: usize,
        distance_metric: HnswDistanceMetric,
    ) -> Result<Vec<(u64, f64)>> {
        let matches = index
            .search(query_f32, k)
            .map_err(|e| anyhow!("HNSW search failed: {}", e))?;

        let n = matches
            .count
            .min(matches.labels.len())
            .min(matches.distances.len());
        // Deduplicate by label, keeping the first (closest) occurrence.
        // usearch 0.21 add() appends duplicate labels on UPDATE, so the
        // graph can contain multiple entries for the same PK. Results are
        // returned in distance order, so the first hit per label is best.
        let mut seen = HashSet::with_capacity(n);
        let mut ranked_labels = Vec::with_capacity(n);
        for i in 0..n {
            if seen.insert(matches.labels[i]) {
                ranked_labels.push((
                    matches.labels[i],
                    distance_metric.normalize_search_distance(matches.distances[i] as f64),
                ));
            }
        }

        Ok(ranked_labels)
    }

    /// Search a shared base graph with semaphore + spawn_blocking.
    ///
    /// The Arc and semaphore permit are moved into the blocking closure,
    /// so both outlive the search even under async cancellation.
    async fn search_shared(
        base: &Arc<SharedHnswIndex>,
        query_f32: &[f32],
        k: usize,
        distance_metric: HnswDistanceMetric,
    ) -> Result<Vec<(u64, f64)>> {
        let permit = hnsw_search_semaphore().acquire().await
            .map_err(|_| anyhow!("HNSW search semaphore closed"))?;
        let base = Arc::clone(base);
        let query = query_f32.to_vec();
        tokio::task::spawn_blocking(move || {
            let _permit = permit; // moved in — released when closure returns
            Self::search_and_rank(&base.index, &query, k, distance_metric)
        }).await
            .map_err(|e| anyhow!("HNSW search task failed: {}", e))?
    }

    /// Search a small per-query delta index with semaphore + spawn_blocking.
    ///
    /// The delta index is wrapped in Arc to transfer ownership safely.
    async fn search_delta(
        delta: &Arc<HnswIndexHandle>,
        query_f32: &[f32],
        k: usize,
        distance_metric: HnswDistanceMetric,
    ) -> Result<Vec<(u64, f64)>> {
        let permit = hnsw_search_semaphore().acquire().await
            .map_err(|_| anyhow!("HNSW search semaphore closed"))?;
        let delta = Arc::clone(delta);
        let query = query_f32.to_vec();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            Self::search_and_rank(&delta, &query, k, distance_metric)
        }).await
            .map_err(|e| anyhow!("HNSW search task failed: {}", e))?
    }
}

#[async_trait]
impl PhysicalOperator for HnswScanOperator {
    fn schema(&self) -> &TableSchema {
        &self.schema
    }

    async fn open(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.row_buffer.clear();
        self.position = 0;
        self.opened = true;

        if self.k == 0 || self.query_vector.is_empty() {
            return Ok(());
        }

        let mut query_f64 = Vec::with_capacity(self.query_vector.len());
        for value in &self.query_vector {
            match value {
                Value::Null => {
                    self.row_buffer.clear();
                    return Ok(());
                }
                Value::Float64(v) => query_f64.push(*v),
                other => {
                    return Err(anyhow!(
                        "HNSW query_vector must contain FLOAT8 values, found {}",
                        other.type_display_name()
                    ));
                }
            }
        }

        let query_f32 = vec_f64_to_f32(&query_f64);

        let ef_search = ctx
            .query_ctx
            .settings_snapshot
            .as_ref()
            .and_then(|s| s.get("hnsw.ef_search"))
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(40);

        // Two-level search: shared base graph + per-query delta index.
        //
        // 1. Get shared base graph from the in-memory index cache (Arc, zero-copy).
        // 2. Scan deltas visible to this transaction's MVCC snapshot.
        // 3. Search base graph + search delta index + merge results.
        //
        // This eliminates the N× memory multiplier: N concurrent queries
        // share one loaded base graph instead of each loading a private copy.
        let keyspace = ctx.store.keyspace().unwrap_or("default");

        // Read meta.
        let meta_key = hnsw_meta_key(ctx.db_id, self.schema.table_id, self.index_id);
        let Some(meta_bytes) = ctx
            .txn
            .get(meta_key)
            .await
            .map_err(|e| anyhow!(e))?
        else {
            return Ok(());
        };
        let meta: HnswMeta =
            serde_json::from_slice(&meta_bytes).map_err(|e| anyhow!(e))?;
        if meta.dropped_at.is_some() {
            return Ok(());
        }
        if meta.storage_version != 1 && meta.storage_version != 2 {
            return Err(anyhow!(
                "HNSW index has unsupported storage_version={}; only v1/v2 supported",
                meta.storage_version
            ));
        }

        let label_mode = meta.label_mode;

        // Scan deltas up to a byte-based budget (adapts to dimensions).
        // If truncated, search with partial deltas — HNSW is already approximate,
        // and missing recent inserts is semantically the same as stale deletions
        // still in the graph. The merge worker will consolidate them into the
        // base graph, at which point they become visible to all queries.
        let delta_limit = max_deltas_for_budget(meta.dimensions, meta.m);
        let (deltas, delta_truncated) = scan_visible_deltas(
            ctx.txn, ctx.db_id, self.schema.table_id, self.index_id,
            delta_limit,
        ).await?;

        if delta_truncated {
            tracing::warn!(
                table = %self.schema.name,
                index = %self.index_name,
                collected = deltas.len(),
                limit = delta_limit,
                "HNSW scan: delta backlog exceeds memory budget; \
                 searching with partial deltas until merge catches up"
            );
        }

        let delta_count = deltas.len();

        // Get shared base graph (cache hit = 0ms, miss = load from disk/S3).
        let shared_base = get_shared_base_graph(
            ctx.txn, ctx.db_id, self.schema.table_id, self.index_id, &meta, keyspace,
        ).await?;

        // Nothing to search.
        if shared_base.is_none() && deltas.is_empty() {
            return Ok(());
        }

        if delta_count > 0 {
            if let Some(m) = crate::worker::get_worker_metrics() {
                m.hnsw_scan_deltas_applied
                    .fetch_add(delta_count as u64, std::sync::atomic::Ordering::Relaxed);
            }
        }

        // Resolve the indexed vector column so we can filter out NULL-vector
        // rows (stale graph entries from UPDATE SET col = NULL).
        let vector_col_idx = self
            .schema
            .indexes
            .iter()
            .find(|idx| idx.id == self.index_id)
            .and_then(|idx| idx.columns.first())
            .and_then(|col_name| self.schema.column_index(col_name));

        // Over-fetch from the HNSW graph to compensate for stale entries
        // (lazy deletion on DELETE / PK-changing UPDATE / SET vec = NULL)
        // and uncommitted vectors that batch_get_rows will filter out.
        // Also respect ef_search as a minimum beam width (approximates
        // pgvector's ef_search behavior since usearch doesn't support
        // per-query ef_search natively).
        //
        // If the first pass yields < k valid rows (too many stale hits),
        // widen fetch_k and retry until we either have enough rows or
        // have exhausted the entire graph.
        // Two-level search: base graph + delta index, merge results.
        let base_graph_size = shared_base.as_ref().map(|s| s.index.size()).unwrap_or(0);
        // Wrap in Arc for safe transfer into spawn_blocking (cancellation-safe).
        let delta_index: Option<Arc<HnswIndexHandle>> = if !deltas.is_empty() {
            Some(Arc::new(build_delta_index(&meta, &deltas)?))
        } else {
            None
        };
        let graph_size = base_graph_size + delta_index.as_ref().map(|d| d.size()).unwrap_or(0);
        let mut fetch_k = self.k.max(ef_search).max(self.k * 2).max(self.k + 100);
        let mut rows;
        let mut distance_by_label: HashMap<u64, f64>;
        let mut pk_to_label: HashMap<String, u64> = HashMap::new();

        loop {
            // Search base graph (if it exists).
            let base_results = if let Some(ref base) = shared_base {
                Self::search_shared(base, &query_f32, fetch_k, self.distance_metric).await?
            } else {
                Vec::new()
            };

            // Search delta index (if it exists) and merge results.
            let ranked_labels = if let Some(ref di) = delta_index {
                let delta_k = fetch_k.min(di.size());
                let delta_results = if delta_k > 0 {
                    Self::search_delta(di, &query_f32, delta_k, self.distance_metric).await?
                } else {
                    Vec::new()
                };
                merge_search_results(&base_results, &delta_results, fetch_k)
            } else {
                base_results
            };
            if ranked_labels.is_empty() {
                self.row_buffer.clear();
                return Ok(());
            }

            // Build rank and distance maps keyed by label (= rowid in Mapped mode).
            let rank_by_label: HashMap<u64, usize> = ranked_labels
                .iter()
                .enumerate()
                .map(|(rank, (label, _))| (*label, rank))
                .collect();
            distance_by_label = ranked_labels.iter().copied().collect();

            // Convert labels → PK values for batch_get_rows.
            pk_to_label.clear();
            let batch_pks: Vec<Vec<Value>> = match label_mode {
                HnswLabelMode::Direct => ranked_labels
                    .iter()
                    .map(|(label, _)| self.pk_value_from_label(*label).map(|pk| vec![pk]))
                    .collect::<Result<Vec<_>>>()?,
                HnswLabelMode::Mapped => {
                    let rowids: Vec<u64> = ranked_labels.iter().map(|(label, _)| *label).collect();
                    let pk_types: Vec<DataType> = self
                        .schema
                        .pk_indices
                        .iter()
                        .map(|&i| self.schema.columns[i].data_type.clone())
                        .collect();
                    let pk_bytes_vec =
                        batch_get_pk_for_rowids(ctx.txn, ctx.db_id, self.schema.table_id, &rowids)
                            .await?;
                    let mut pks = Vec::with_capacity(rowids.len());
                    for (i, opt_bytes) in pk_bytes_vec.into_iter().enumerate() {
                        let Some(pk_bytes) = opt_bytes else {
                            // Stale label — row was deleted, mapping removed.
                            continue;
                        };
                        let pk_values = decode_pk_from_index_suffix(&pk_bytes, &pk_types)?;
                        // INVARIANT: pk_key must use the same Value::to_string()
                        // format here and in the sort/distance blocks below.
                        // If Value's Display impl changes, both sites must stay
                        // in sync or the HashMap lookup will silently miss.
                        let pk_key = pk_values
                            .iter()
                            .map(|v| v.to_string())
                            .collect::<Vec<_>>()
                            .join(",");
                        pk_to_label.insert(pk_key, rowids[i]);
                        pks.push(pk_values);
                    }
                    pks
                }
            };

            let fetched_rows = ctx
                .store
                .batch_get_rows(
                    ctx.txn,
                    ctx.db_id,
                    self.schema.table_id,
                    batch_pks,
                    &self.schema,
                )
                .await?;

            let mut valid = Vec::with_capacity(fetched_rows.len());
            for mut r in fetched_rows {
                fill_row_defaults(&mut r, &self.schema)?;
                while r.values.len() < self.schema.columns.len() {
                    r.values.push(Value::Null);
                }
                // Discard rows whose indexed vector column is NULL — stale
                // graph entries left by UPDATE ... SET vec = NULL (usearch
                // has no remove, so the old label persists in the graph).
                if let Some(vi) = vector_col_idx {
                    if matches!(r.values.get(vi), Some(Value::Null) | None) {
                        continue;
                    }
                }
                valid.push(r);
            }

            // Sort by HNSW search rank (closest first).
            let pk_to_label_ref = &pk_to_label;
            valid.sort_by_key(|row| {
                let pk_col_idx = self.schema.pk_indices.first().copied().unwrap_or(0);
                let label = match label_mode {
                    HnswLabelMode::Direct => row.values.get(pk_col_idx).and_then(Self::pk_as_u64),
                    HnswLabelMode::Mapped => {
                        let pk_key = row
                            .values
                            .get(pk_col_idx)
                            .map(|v| v.to_string())
                            .unwrap_or_default();
                        pk_to_label_ref.get(&pk_key).copied()
                    }
                };
                label
                    .and_then(|l| rank_by_label.get(&l).copied())
                    .unwrap_or(usize::MAX)
            });

            valid.truncate(self.k);
            rows = valid;

            // Enough valid rows, or we've already searched the entire graph.
            if rows.len() >= self.k || fetch_k >= graph_size {
                break;
            }

            // Widen search: double fetch_k, capped at graph size.
            fetch_k = (fetch_k.saturating_mul(2)).min(graph_size);
        }

        if self.distance_expr.is_some() {
            let pk_to_label_ref = &pk_to_label;
            for row in &mut rows {
                let pk_col_idx = self.schema.pk_indices.first().copied().unwrap_or(0);
                let label = match label_mode {
                    HnswLabelMode::Direct => row.values.get(pk_col_idx).and_then(Self::pk_as_u64),
                    HnswLabelMode::Mapped => {
                        let pk_key = row
                            .values
                            .get(pk_col_idx)
                            .map(|v| v.to_string())
                            .unwrap_or_default();
                        pk_to_label_ref.get(&pk_key).copied()
                    }
                };
                let distance = label
                    .and_then(|l| distance_by_label.get(&l).copied())
                    .unwrap_or(f64::NAN);
                row.values.push(Value::Float64(distance));
            }
        }

        self.row_buffer = rows;
        Ok(())
    }

    async fn next(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>> {
        if !self.opened {
            return Err(anyhow!("HnswScanOperator not opened"));
        }

        if self.position >= self.row_buffer.len() {
            return Ok(None);
        }

        let row = self.row_buffer[self.position].clone();
        self.position += 1;
        Ok(Some(row))
    }

    async fn close(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.row_buffer.clear();
        self.opened = false;
        Ok(())
    }

    fn name(&self) -> &'static str {
        "HnswScan"
    }

    fn explain_info(&self) -> Option<String> {
        Some(format!(
            "table={}, index={}, metric={}, k={}",
            self.schema.name,
            self.index_name,
            self.distance_metric.as_str(),
            self.k
        ))
    }
}
