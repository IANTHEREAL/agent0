use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};

use super::{ExecutionContext, PhysicalOperator};
use crate::model::{DataType, Row, TableSchema, Value};
use crate::sql::analyzer::types::TypedExpr;
use crate::sql::hnsw::storage::{
    batch_get_pk_for_rowids, load_hnsw_graph_with_deltas,
};
use crate::sql::hnsw::{vec_f64_to_f32, HnswDistanceMetric, HnswIndexHandle, HnswLabelMode};
use crate::storage::decode_pk_from_index_suffix;
use crate::sql::projection::fill_row_defaults;

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

    fn search_ranked_labels(
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

        // Load base graph + apply pending deltas so read-your-writes holds:
        // delta keys written by prior INSERT/UPDATE in the same txn are
        // visible via txn.scan's buffer merge.
        let Some((hnsw_index, meta, delta_count)) =
            load_hnsw_graph_with_deltas(ctx.txn, ctx.db_id, self.schema.table_id, self.index_id)
                .await?
        else {
            return Ok(());
        };
        let label_mode = meta.label_mode;
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
        let graph_size = hnsw_index.size();
        let mut fetch_k = self.k.max(ef_search).max(self.k * 2).max(self.k + 100);
        let mut rows;
        let mut distance_by_label: HashMap<u64, f64>;
        let mut pk_to_label: HashMap<String, u64> = HashMap::new();

        loop {
            let ranked_labels =
                Self::search_ranked_labels(&hnsw_index, &query_f32, fetch_k, self.distance_metric)?;
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
                HnswLabelMode::Direct => {
                    ranked_labels
                        .iter()
                        .map(|(label, _)| {
                            self.pk_value_from_label(*label).map(|pk| vec![pk])
                        })
                        .collect::<Result<Vec<_>>>()?
                }
                HnswLabelMode::Mapped => {
                    let rowids: Vec<u64> =
                        ranked_labels.iter().map(|(label, _)| *label).collect();
                    let pk_types: Vec<DataType> = self
                        .schema
                        .pk_indices
                        .iter()
                        .map(|&i| self.schema.columns[i].data_type.clone())
                        .collect();
                    let pk_bytes_vec = batch_get_pk_for_rowids(
                        ctx.txn,
                        ctx.db_id,
                        self.schema.table_id,
                        &rowids,
                    )
                    .await?;
                    let mut pks = Vec::with_capacity(rowids.len());
                    for (i, opt_bytes) in pk_bytes_vec.into_iter().enumerate() {
                        let Some(pk_bytes) = opt_bytes else {
                            // Stale label — row was deleted, mapping removed.
                            continue;
                        };
                        let pk_values =
                            decode_pk_from_index_suffix(&pk_bytes, &pk_types)?;
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
                    HnswLabelMode::Direct => row
                        .values
                        .get(pk_col_idx)
                        .and_then(Self::pk_as_u64),
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
                    HnswLabelMode::Direct => row
                        .values
                        .get(pk_col_idx)
                        .and_then(Self::pk_as_u64),
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
