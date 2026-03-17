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
use std::collections::{BTreeSet, HashMap, HashSet};
use tracing::debug;

use super::{ExecutionContext, PhysicalOperator};
use crate::model::{Row, TableSchema, Value};
use crate::sql::planner::GinQual;
use crate::sql::projection::fill_row_defaults;
use crate::storage::decode_pk_from_index_suffix;

const GIN_BATCH_FETCH_SIZE: usize = 256;
const GIN_POSTING_PAGE_SIZE: u32 = 4096;
const GIN_PROBE_LIMIT: u32 = 1024;

#[derive(Debug)]
enum GinCandidateSet {
    Keys(BTreeSet<Vec<u8>>),
    AllDocs,
}

#[derive(Debug, Default, Clone)]
struct GinScanMetrics {
    term_count: u32,
    posting_scan_rpcs: u32,
    posting_pairs_scanned: u64,
    membership_probe_rpcs: u32,
    membership_probe_keys: u64,
    candidate_pk_count: u64,
    row_batch_get_rpcs: u32,
    rows_fetched: u64,
    rows_output: u64,
}

#[derive(Debug)]
pub struct GinScanOperator {
    schema: TableSchema,
    index_id: u64,
    #[allow(dead_code)]
    index_name: String,
    qual: GinQual,
    scan_limit: Option<usize>,
    /// Primary-key queue produced by posting-list set operations.
    pk_queue: Vec<Vec<Value>>,
    /// Row buffer for batch_get results.
    row_buffer: Vec<Row>,
    position: usize,
    opened: bool,
    metrics: GinScanMetrics,
}

impl GinScanOperator {
    pub fn new(
        schema: TableSchema,
        index_id: u64,
        index_name: String,
        qual: GinQual,
        scan_limit: Option<usize>,
    ) -> Self {
        Self {
            schema,
            index_id,
            index_name,
            qual,
            scan_limit,
            pk_queue: Vec::new(),
            row_buffer: Vec::new(),
            position: 0,
            opened: false,
            metrics: GinScanMetrics::default(),
        }
    }

    fn gin_qual_term_count(qual: &GinQual) -> u32 {
        match qual {
            GinQual::Term { .. } => 1,
            GinQual::And(children) | GinQual::Or(children) => {
                children.iter().map(Self::gin_qual_term_count).sum()
            }
            GinQual::Not(inner) => Self::gin_qual_term_count(inner),
        }
    }

    /// Resolve PK column types from the schema.
    fn pk_types(&self) -> Vec<crate::model::DataType> {
        if self.schema.pk_indices.is_empty() {
            vec![super::scan::IMPLICIT_PK_TYPE]
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

    fn collect_flat_and_terms(
        qual: &GinQual,
        positive: &mut Vec<u64>,
        negative: &mut Vec<u64>,
    ) -> bool {
        match qual {
            GinQual::Term { token_hash } => {
                positive.push(*token_hash);
                true
            }
            GinQual::And(children) => children
                .iter()
                .all(|child| Self::collect_flat_and_terms(child, positive, negative)),
            GinQual::Not(inner) => match inner.as_ref() {
                GinQual::Term { token_hash } => {
                    negative.push(*token_hash);
                    true
                }
                _ => false,
            },
            GinQual::Or(_) => false,
        }
    }

    fn dedup_in_order(tokens: Vec<u64>) -> Vec<u64> {
        let mut seen = HashSet::with_capacity(tokens.len());
        tokens
            .into_iter()
            .filter(|token| seen.insert(*token))
            .collect()
    }

    async fn probe_posting_list_size(
        &mut self,
        ctx: &mut ExecutionContext<'_>,
        token_hash: u64,
        probe_cache: &mut HashMap<u64, (u32, bool)>,
    ) -> Result<(u32, bool)> {
        if let Some(cached) = probe_cache.get(&token_hash).copied() {
            return Ok(cached);
        }

        self.metrics.posting_scan_rpcs += 1;
        let probe = ctx
            .store
            .probe_gin_posting_list_size(
                ctx.txn,
                ctx.db_id,
                self.schema.table_id,
                self.index_id,
                token_hash,
                GIN_PROBE_LIMIT,
            )
            .await?;
        self.metrics.posting_pairs_scanned += probe.0 as u64;
        probe_cache.insert(token_hash, probe);
        Ok(probe)
    }

    async fn scan_posting_list_page(
        &mut self,
        ctx: &mut ExecutionContext<'_>,
        token_hash: u64,
        cursor: Option<&[u8]>,
    ) -> Result<(Vec<Vec<u8>>, Option<Vec<u8>>)> {
        self.metrics.posting_scan_rpcs += 1;
        let (page, next_cursor) = ctx
            .store
            .scan_gin_posting_list_page(
                ctx.txn,
                ctx.db_id,
                self.schema.table_id,
                self.index_id,
                token_hash,
                cursor,
                GIN_POSTING_PAGE_SIZE,
            )
            .await?;
        self.metrics.posting_pairs_scanned += page.len() as u64;
        Ok((page, next_cursor))
    }

    async fn filter_posting_membership(
        &mut self,
        ctx: &mut ExecutionContext<'_>,
        token_hash: u64,
        candidates: &[Vec<u8>],
    ) -> Result<Vec<Vec<u8>>> {
        self.metrics.membership_probe_rpcs += 1;
        self.metrics.membership_probe_keys += candidates.len() as u64;
        ctx.store
            .filter_gin_posting_membership(
                ctx.txn,
                ctx.db_id,
                self.schema.table_id,
                self.index_id,
                token_hash,
                candidates,
            )
            .await
    }

    async fn evaluate_flat_and_driver_probe(
        &mut self,
        ctx: &mut ExecutionContext<'_>,
    ) -> Result<Option<GinCandidateSet>> {
        let mut positive = Vec::new();
        let mut negative = Vec::new();
        if !Self::collect_flat_and_terms(&self.qual, &mut positive, &mut negative) {
            return Ok(None);
        }

        positive = Self::dedup_in_order(positive);
        negative = Self::dedup_in_order(negative);
        if positive.is_empty() {
            return Ok(Some(GinCandidateSet::AllDocs));
        }

        let mut probe_cache = HashMap::new();
        let mut estimates = Vec::with_capacity(positive.len());
        for token_hash in &positive {
            let (count, saturated) = self
                .probe_posting_list_size(ctx, *token_hash, &mut probe_cache)
                .await?;
            estimates.push((*token_hash, count, saturated));
        }
        estimates.sort_by_key(|(_, count, saturated)| (*saturated, *count));

        let driver_token = estimates[0].0;
        let probe_tokens: Vec<u64> = estimates
            .iter()
            .skip(1)
            .map(|(token, _, _)| *token)
            .collect();

        let effective_limit = self.scan_limit.unwrap_or(usize::MAX);
        let mut matched = BTreeSet::new();
        let mut cursor: Option<Vec<u8>> = None;

        loop {
            let (page, next_cursor) = self
                .scan_posting_list_page(ctx, driver_token, cursor.as_deref())
                .await?;
            if page.is_empty() {
                break;
            }

            let mut candidates = page;
            for token_hash in &probe_tokens {
                if candidates.is_empty() {
                    break;
                }
                candidates = self
                    .filter_posting_membership(ctx, *token_hash, &candidates)
                    .await?;
            }

            for token_hash in &negative {
                if candidates.is_empty() {
                    break;
                }
                let excluded = self
                    .filter_posting_membership(ctx, *token_hash, &candidates)
                    .await?;
                if excluded.is_empty() {
                    continue;
                }
                let excluded_set: HashSet<Vec<u8>> = excluded.into_iter().collect();
                candidates.retain(|pk_bytes| !excluded_set.contains(pk_bytes));
            }

            for pk_bytes in candidates {
                matched.insert(pk_bytes);
                if matched.len() >= effective_limit {
                    break;
                }
            }

            if matched.len() >= effective_limit {
                break;
            }

            match next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }

        Ok(Some(GinCandidateSet::Keys(matched)))
    }

    /// Recursively evaluate a `GinQual` tree against the TiKV store.
    ///
    /// Returns a **sorted, deduplicated** set of raw PK byte vectors.
    async fn evaluate_qual(
        &mut self,
        qual: &GinQual,
        ctx: &mut ExecutionContext<'_>,
        table_id: u64,
        index_id: u64,
    ) -> Result<GinCandidateSet> {
        match qual {
            GinQual::Term { token_hash } => {
                self.metrics.posting_scan_rpcs += 1;
                let pk_list = ctx
                    .store
                    .scan_gin_posting_list(ctx.txn, ctx.db_id, table_id, index_id, *token_hash)
                    .await?;
                self.metrics.posting_pairs_scanned += pk_list.len() as u64;
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
                let mut result = Box::pin(Self::evaluate_qual(
                    self,
                    positive[0],
                    ctx,
                    table_id,
                    index_id,
                ))
                .await?;
                for child in &positive[1..] {
                    let other =
                        Box::pin(Self::evaluate_qual(self, child, ctx, table_id, index_id)).await?;
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
                    let exclude = Box::pin(Self::evaluate_qual(
                        self, neg_child, ctx, table_id, index_id,
                    ))
                    .await?;
                    result = Self::subtract_candidates(result, exclude);
                }

                Ok(result)
            }

            GinQual::Or(children) => {
                let mut result = GinCandidateSet::Keys(BTreeSet::new());
                for child in children {
                    let child_set =
                        Box::pin(Self::evaluate_qual(self, child, ctx, table_id, index_id)).await?;
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
            self.metrics.row_batch_get_rpcs += 1;
            self.metrics.rows_fetched += batch_pks.len() as u64;
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
        self.metrics = GinScanMetrics {
            term_count: Self::gin_qual_term_count(&self.qual),
            ..GinScanMetrics::default()
        };

        // Use the flat-AND driver/probe fast path when possible; otherwise fall
        // back to the existing recursive posting-list evaluation.
        let candidate_pk_set = match self.evaluate_flat_and_driver_probe(ctx).await? {
            Some(candidate_pk_set) => candidate_pk_set,
            None => {
                let qual = self.qual.clone();
                self.evaluate_qual(&qual, ctx, self.schema.table_id, self.index_id)
                    .await?
            }
        };

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
                // Conservative fallback: scan primary-key bytes only instead of
                // materializing full rows for the whole table.
                let pk_types = self.pk_types();
                let effective_limit = self.scan_limit.unwrap_or(usize::MAX);
                let mut cursor: Option<Vec<u8>> = None;
                while self.pk_queue.len() < effective_limit {
                    self.metrics.posting_scan_rpcs += 1;
                    let page_size = effective_limit
                        .saturating_sub(self.pk_queue.len())
                        .min(GIN_POSTING_PAGE_SIZE as usize)
                        .max(1) as u32;
                    let (page, next_cursor) = ctx
                        .store
                        .scan_table_primary_key_bytes_page(
                            ctx.txn,
                            ctx.db_id,
                            self.schema.table_id,
                            cursor.as_deref(),
                            page_size,
                        )
                        .await?;
                    self.metrics.posting_pairs_scanned += page.len() as u64;
                    if page.is_empty() {
                        break;
                    }

                    for pk_bytes in page {
                        let pk = decode_pk_from_index_suffix(&pk_bytes, &pk_types)?;
                        self.pk_queue.push(pk);
                        if self.pk_queue.len() >= effective_limit {
                            break;
                        }
                    }

                    match next_cursor {
                        Some(next) if self.pk_queue.len() < effective_limit => {
                            cursor = Some(next);
                        }
                        _ => break,
                    }
                }
            }
        }
        self.metrics.candidate_pk_count = self.pk_queue.len() as u64;

        debug!(
            table = %self.schema.name,
            index = %self.index_name,
            term_count = self.metrics.term_count,
            posting_scan_rpcs = self.metrics.posting_scan_rpcs,
            posting_pairs_scanned = self.metrics.posting_pairs_scanned,
            membership_probe_rpcs = self.metrics.membership_probe_rpcs,
            membership_probe_keys = self.metrics.membership_probe_keys,
            candidate_pk_count = self.metrics.candidate_pk_count,
            scan_limit = self.scan_limit,
            "gin_scan_open"
        );

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
            self.metrics.rows_output += 1;
            Ok(Some(row))
        } else {
            self.load_next_batch(ctx).await?;
            if self.position < self.row_buffer.len() {
                let row = self.row_buffer[self.position].clone();
                self.position += 1;
                self.metrics.rows_output += 1;
                Ok(Some(row))
            } else {
                Ok(None)
            }
        }
    }

    async fn close(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<()> {
        debug!(
            table = %self.schema.name,
            index = %self.index_name,
            term_count = self.metrics.term_count,
            posting_scan_rpcs = self.metrics.posting_scan_rpcs,
            posting_pairs_scanned = self.metrics.posting_pairs_scanned,
            membership_probe_rpcs = self.metrics.membership_probe_rpcs,
            membership_probe_keys = self.metrics.membership_probe_keys,
            candidate_pk_count = self.metrics.candidate_pk_count,
            row_batch_get_rpcs = self.metrics.row_batch_get_rpcs,
            rows_fetched = self.metrics.rows_fetched,
            rows_output = self.metrics.rows_output,
            scan_limit = self.scan_limit,
            "gin_scan_close"
        );
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

    #[test]
    fn test_collect_flat_and_terms_accepts_terms_and_not_terms() {
        let qual = GinQual::And(vec![
            GinQual::Term { token_hash: 11 },
            GinQual::Not(Box::new(GinQual::Term { token_hash: 22 })),
            GinQual::Term { token_hash: 33 },
        ]);
        let mut positive = Vec::new();
        let mut negative = Vec::new();
        assert!(GinScanOperator::collect_flat_and_terms(
            &qual,
            &mut positive,
            &mut negative
        ));
        assert_eq!(positive, vec![11, 33]);
        assert_eq!(negative, vec![22]);
    }

    #[test]
    fn test_collect_flat_and_terms_rejects_or_children() {
        let qual = GinQual::And(vec![
            GinQual::Term { token_hash: 11 },
            GinQual::Or(vec![
                GinQual::Term { token_hash: 22 },
                GinQual::Term { token_hash: 33 },
            ]),
        ]);
        let mut positive = Vec::new();
        let mut negative = Vec::new();
        assert!(!GinScanOperator::collect_flat_and_terms(
            &qual,
            &mut positive,
            &mut negative
        ));
    }
}
