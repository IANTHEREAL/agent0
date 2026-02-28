use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::collections::HashMap;

use super::{ExecutionContext, PhysicalOperator};
use crate::model::{DataType, Row, TableSchema, Value};
use crate::sql::analyzer::types::TypedExpr;
use crate::sql::hnsw::storage::load_hnsw_graph_from_txn;
use crate::sql::hnsw::{vec_f64_to_f32, HnswIndexHandle};
use crate::sql::projection::fill_row_defaults;

#[allow(dead_code)] // fields used in explain_info() trait method
#[derive(Debug)]
pub struct HnswScanOperator {
    schema: TableSchema,
    index_id: u64,
    index_name: String,
    query_vector: Vec<Value>,
    k: usize,
    distance_metric: String,
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
        distance_metric: String,
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
    ) -> Result<Vec<(u64, f64)>> {
        let matches = index
            .search(query_f32, k)
            .map_err(|e| anyhow!("HNSW search failed: {}", e))?;

        let n = matches
            .count
            .min(matches.labels.len())
            .min(matches.distances.len());
        let mut ranked_labels = Vec::with_capacity(n);
        for i in 0..n {
            ranked_labels.push((matches.labels[i], matches.distances[i] as f64));
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

        // Load graph from the session transaction so that read-your-writes
        // holds: vectors written by prior INSERT/UPDATE in the same txn are
        // visible to this scan (txn.get checks the local write buffer first).
        let Some((hnsw_index, _meta)) =
            load_hnsw_graph_from_txn(ctx.txn, ctx.db_id, self.schema.table_id, self.index_id)
                .await?
        else {
            return Ok(());
        };

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

        loop {
            let ranked_labels = Self::search_ranked_labels(&hnsw_index, &query_f32, fetch_k)?;
            if ranked_labels.is_empty() {
                self.row_buffer.clear();
                return Ok(());
            }

            let batch_pks = ranked_labels
                .iter()
                .map(|(label, _)| self.pk_value_from_label(*label).map(|pk| vec![pk]))
                .collect::<Result<Vec<_>>>()?;

            let rank_by_label: HashMap<u64, usize> = ranked_labels
                .iter()
                .enumerate()
                .map(|(rank, (label, _))| (*label, rank))
                .collect();
            distance_by_label = ranked_labels.iter().copied().collect();

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

            valid.sort_by_key(|row| {
                let pk_col_idx = self.schema.pk_indices.first().copied().unwrap_or(0);
                row.values
                    .get(pk_col_idx)
                    .and_then(Self::pk_as_u64)
                    .and_then(|label| rank_by_label.get(&label).copied())
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
            for row in &mut rows {
                let pk_col_idx = self.schema.pk_indices.first().copied().unwrap_or(0);
                let distance = row
                    .values
                    .get(pk_col_idx)
                    .and_then(Self::pk_as_u64)
                    .and_then(|label| distance_by_label.get(&label).copied())
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
            self.schema.name, self.index_name, self.distance_metric, self.k
        ))
    }
}
