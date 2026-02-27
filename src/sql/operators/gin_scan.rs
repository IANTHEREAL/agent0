//! GIN index scan operator.
//!
//! Walks a [`GinQual`] boolean tree, scanning posting lists from TiKV and
//! performing set operations (intersect / union / difference) on the resulting
//! PK byte vectors.  The candidate PKs are then fed through the standard
//! `IndexScanBase` pipeline for batch_get → row materialisation.
//!
//! **Correctness contract**: the resulting candidate set is always a *superset*
//! of the true result (no false negatives).  A recheck filter must be applied
//! on top to remove false positives.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::collections::BTreeSet;

use super::{ExecutionContext, PhysicalOperator};
use crate::model::{Row, TableSchema, Value};
use crate::sql::planner::GinQual;
use crate::sql::projection::fill_row_defaults;
use crate::storage::decode_pk_from_index_suffix;

const GIN_BATCH_FETCH_SIZE: usize = 256;

#[derive(Debug)]
enum GinCandidateSet {
    Keys(BTreeSet<Vec<u8>>),
    AllDocs,
}

#[derive(Debug)]
pub struct GinScanOperator {
    schema: TableSchema,
    index_id: u64,
    #[allow(dead_code)]
    index_name: String,
    qual: GinQual,
    /// Primary-key queue produced by posting-list set operations.
    pk_queue: Vec<Vec<Value>>,
    /// Row buffer for batch_get results.
    row_buffer: Vec<Row>,
    position: usize,
    opened: bool,
}

impl GinScanOperator {
    pub fn new(schema: TableSchema, index_id: u64, index_name: String, qual: GinQual) -> Self {
        Self {
            schema,
            index_id,
            index_name,
            qual,
            pk_queue: Vec::new(),
            row_buffer: Vec::new(),
            position: 0,
            opened: false,
        }
    }

    /// Resolve PK column types from the schema.
    fn pk_types(&self) -> Vec<crate::model::DataType> {
        if self.schema.pk_indices.is_empty() {
            vec![crate::model::DataType::Uuid]
        } else {
            self.schema
                .pk_indices
                .iter()
                .map(|&idx| self.schema.columns[idx].data_type.clone())
                .collect()
        }
    }

    fn subtract_candidates(base: GinCandidateSet, exclude: GinCandidateSet) -> GinCandidateSet {
        match (base, exclude) {
            (GinCandidateSet::AllDocs, _) => GinCandidateSet::AllDocs,
            (GinCandidateSet::Keys(lhs), GinCandidateSet::AllDocs) => GinCandidateSet::Keys(lhs),
            (GinCandidateSet::Keys(lhs), GinCandidateSet::Keys(rhs)) => {
                GinCandidateSet::Keys(lhs.difference(&rhs).cloned().collect())
            }
        }
    }

    /// Recursively evaluate a `GinQual` tree against the TiKV store.
    ///
    /// Returns a **sorted, deduplicated** set of raw PK byte vectors.
    async fn evaluate_qual(
        qual: &GinQual,
        ctx: &mut ExecutionContext<'_>,
        table_id: u64,
        index_id: u64,
    ) -> Result<GinCandidateSet> {
        match qual {
            GinQual::Term { token_hash } => {
                let pk_list = ctx
                    .store
                    .scan_gin_posting_list(ctx.txn, ctx.db_id, table_id, index_id, *token_hash)
                    .await?;
                Ok(GinCandidateSet::Keys(pk_list.into_iter().collect()))
            }

            GinQual::And(children) => {
                if children.is_empty() {
                    return Ok(GinCandidateSet::Keys(BTreeSet::new()));
                }

                // Separate positive children (Term, And, Or) from Not children.
                let mut positive = Vec::new();
                let mut negative = Vec::new();
                for child in children {
                    if let GinQual::Not(inner) = child {
                        negative.push(inner.as_ref());
                    } else {
                        positive.push(child);
                    }
                }

                if positive.is_empty() {
                    // Pure negation cannot be represented by posting-list operations.
                    // Return full-table candidates and rely on recheck.
                    return Ok(GinCandidateSet::AllDocs);
                }

                // Start with first positive child, then intersect the rest.
                let mut result =
                    Box::pin(Self::evaluate_qual(positive[0], ctx, table_id, index_id)).await?;
                for child in &positive[1..] {
                    let other =
                        Box::pin(Self::evaluate_qual(child, ctx, table_id, index_id)).await?;
                    result = match (result, other) {
                        (GinCandidateSet::AllDocs, rhs) => rhs,
                        (lhs, GinCandidateSet::AllDocs) => lhs,
                        (GinCandidateSet::Keys(lhs), GinCandidateSet::Keys(rhs)) => {
                            GinCandidateSet::Keys(lhs.intersection(&rhs).cloned().collect())
                        }
                    };
                }

                // Subtract negative children.
                for neg_child in negative {
                    let exclude =
                        Box::pin(Self::evaluate_qual(neg_child, ctx, table_id, index_id)).await?;
                    result = Self::subtract_candidates(result, exclude);
                }

                Ok(result)
            }

            GinQual::Or(children) => {
                let mut result = GinCandidateSet::Keys(BTreeSet::new());
                for child in children {
                    let child_set =
                        Box::pin(Self::evaluate_qual(child, ctx, table_id, index_id)).await?;
                    result = match (result, child_set) {
                        (GinCandidateSet::AllDocs, _) | (_, GinCandidateSet::AllDocs) => {
                            GinCandidateSet::AllDocs
                        }
                        (GinCandidateSet::Keys(lhs), GinCandidateSet::Keys(rhs)) => {
                            GinCandidateSet::Keys(lhs.union(&rhs).cloned().collect())
                        }
                    };
                }
                Ok(result)
            }

            GinQual::Not(_) => {
                // Negation is not directly representable from posting lists alone.
                // Return full-table candidates (superset) and rely on recheck.
                Ok(GinCandidateSet::AllDocs)
            }
        }
    }

    /// Fetch the next batch of rows from `pk_queue` via batch_get.
    async fn load_next_batch(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.row_buffer.clear();
        self.position = 0;

        while self.row_buffer.is_empty() && !self.pk_queue.is_empty() {
            let batch_size = GIN_BATCH_FETCH_SIZE.min(self.pk_queue.len());
            let batch_pks: Vec<Vec<Value>> = self.pk_queue.drain(..batch_size).collect();
            let rows = ctx
                .store
                .batch_get_rows(
                    ctx.txn,
                    ctx.db_id,
                    self.schema.table_id,
                    batch_pks,
                    &self.schema,
                )
                .await?;

            self.row_buffer = rows
                .into_iter()
                .map(|mut r| {
                    fill_row_defaults(&mut r, &self.schema)?;
                    while r.values.len() < self.schema.columns.len() {
                        r.values.push(Value::Null);
                    }
                    Ok(r)
                })
                .collect::<Result<Vec<_>>>()?;
        }

        Ok(())
    }
}

#[async_trait]
impl PhysicalOperator for GinScanOperator {
    fn schema(&self) -> &TableSchema {
        &self.schema
    }

    async fn open(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.pk_queue.clear();
        self.row_buffer.clear();
        self.position = 0;
        self.opened = true;

        // Evaluate the GinQual tree to get candidate PK bytes.
        let candidate_pk_set =
            Self::evaluate_qual(&self.qual, ctx, self.schema.table_id, self.index_id).await?;

        match candidate_pk_set {
            GinCandidateSet::Keys(candidate_pk_bytes) => {
                // Decode raw PK bytes into typed Values.
                let pk_types = self.pk_types();
                self.pk_queue.reserve(candidate_pk_bytes.len());
                for pk_bytes in candidate_pk_bytes {
                    let pk = decode_pk_from_index_suffix(&pk_bytes, &pk_types)?;
                    self.pk_queue.push(pk);
                }
            }
            GinCandidateSet::AllDocs => {
                // Conservative fallback: scan all rows and let the recheck filter
                // enforce exact boolean semantics.
                let rows = ctx
                    .store
                    .scan(ctx.txn, ctx.db_id, &self.schema.name, None)
                    .await?;
                self.pk_queue.reserve(rows.len());
                for row in rows {
                    let pk_values = if self.schema.pk_indices.is_empty() {
                        let Some(first) = row.values.first() else {
                            continue;
                        };
                        vec![first.clone()]
                    } else {
                        let mut values = Vec::with_capacity(self.schema.pk_indices.len());
                        let mut missing_pk = false;
                        for &idx in &self.schema.pk_indices {
                            if let Some(v) = row.values.get(idx) {
                                values.push(v.clone());
                            } else {
                                missing_pk = true;
                                break;
                            }
                        }
                        if missing_pk {
                            continue;
                        }
                        values
                    };
                    self.pk_queue.push(pk_values);
                }
            }
        }

        // Prime the first batch.
        self.load_next_batch(ctx).await?;
        Ok(())
    }

    async fn next(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>> {
        if !self.opened {
            return Err(anyhow!("GinScanOperator not opened"));
        }

        if self.position < self.row_buffer.len() {
            let row = self.row_buffer[self.position].clone();
            self.position += 1;
            Ok(Some(row))
        } else {
            self.load_next_batch(ctx).await?;
            if self.position < self.row_buffer.len() {
                let row = self.row_buffer[self.position].clone();
                self.position += 1;
                Ok(Some(row))
            } else {
                Ok(None)
            }
        }
    }

    async fn close(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.pk_queue.clear();
        self.row_buffer.clear();
        self.opened = false;
        Ok(())
    }

    fn name(&self) -> &'static str {
        "GinScan"
    }

    fn explain_info(&self) -> Option<String> {
        Some(format!(
            "table={}, index={}",
            self.schema.name, self.index_name
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_subtract_candidates_keys_minus_all_docs_keeps_keys() {
        let mut keys = BTreeSet::new();
        keys.insert(vec![1]);
        keys.insert(vec![2]);

        let result = GinScanOperator::subtract_candidates(
            GinCandidateSet::Keys(keys),
            GinCandidateSet::AllDocs,
        );

        match result {
            GinCandidateSet::Keys(keys) => {
                assert_eq!(keys.len(), 2);
                assert!(keys.contains(&vec![1]));
                assert!(keys.contains(&vec![2]));
            }
            other => panic!("expected Keys, got {:?}", other),
        }
    }
}
