//! Unified expression evaluation boundary for the analyzed SELECT executor.
//!
//! `ExprRuntime` wraps the three-phase expression lifecycle:
//! - **Phase 1 (batch):** pre-materialize uncorrelated subqueries / sequences
//! - **Phase 2 (sync):** pure `eval_typed_expr` (hot path for operators)
//! - **Phase 3 (per-row async):** materialize correlated subqueries / catalog funcs, then eval
//!
//! The executor calls `ExprRuntime` methods instead of hand-writing per-row loops.
//! Operators are NOT affected — they continue calling `eval_typed_expr` directly.

use crate::model::{Row, TableSchema, Value};
use crate::sql::analyzer::types::TypedExpr;
use crate::sql::executor::core::Executor;
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::operators::{
    detect_srf, eval_srf, execute_operator_tree, execute_operator_tree_with_ctes, BoxedOperator,
    SrfKind,
};
use crate::sql::query_context::QueryContext;

use anyhow::Result;
use std::collections::HashMap;
use tikv_client::Transaction;

// ── ExprRuntime ──────────────────────────────────────────────────

pub(super) struct ExprRuntime<'a> {
    executor: &'a Executor,
    db_id: u64,
    search_path: &'a [String],
    ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    qctx: QueryContext,
}

impl<'a> ExprRuntime<'a> {
    pub fn new(
        executor: &'a Executor,
        db_id: u64,
        search_path: &'a [String],
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Self {
        Self {
            executor,
            db_id,
            search_path,
            ctes,
            qctx: QueryContext::from_task_locals(),
        }
    }

    // ── Phase 2: Sync evaluation ────────────────────────────────

    /// Pure sync evaluation (hot path). Wraps `eval_typed_expr`.
    ///
    /// Will be wired when mod.rs hand-written loops are replaced.
    #[inline]
    #[allow(dead_code)] // framework: ExprRuntime field
    pub fn eval(&self, expr: &TypedExpr, row: &Row) -> Result<Value> {
        eval_typed_expr(expr, row, &self.qctx)
    }

    // ── Phase 3: Per-row async evaluation ───────────────────────

    /// Materialize correlated subqueries/catalog funcs for a row, then evaluate.
    ///
    /// Will be wired when mod.rs hand-written loops are replaced.
    #[allow(dead_code)] // framework: ExprRuntime field
    pub async fn resolve_and_eval(
        &self,
        expr: &TypedExpr,
        row: &Row,
        schema: &TableSchema,
        txn: &mut Transaction,
        seq: &mut HashMap<String, i64>,
    ) -> Result<Value> {
        let materialized = self
            .executor
            .materialize_expr_for_row(
                expr,
                row,
                None,
                Some(schema),
                txn,
                self.db_id,
                seq,
                self.search_path,
                self.ctes,
                &self.qctx,
            )
            .await?;
        eval_typed_expr(&materialized, row, &self.qctx)
    }

    // ── Composite operations ────────────────────────────────────

    /// Project rows: evaluate projection expressions per-row.
    /// Auto-selects sync or async per-expression.
    pub async fn project_rows(
        &self,
        rows: Vec<Row>,
        exprs: &[TypedExpr],
        schema: &TableSchema,
        txn: &mut Transaction,
        seq: &mut HashMap<String, i64>,
    ) -> Result<Vec<Row>> {
        // Pre-detect SRF expressions (UNNEST, regexp_split_to_table, etc.).
        let srf_indices: Vec<(usize, SrfKind)> = exprs
            .iter()
            .enumerate()
            .filter_map(|(i, expr)| detect_srf(expr).map(|kind| (i, kind)))
            .collect();
        let has_srf = !srf_indices.is_empty();

        let any_async = exprs.iter().any(needs_async);
        if any_async {
            let mut projected = Vec::with_capacity(rows.len());
            for row in &rows {
                let mut values = Vec::with_capacity(exprs.len());
                for expr in exprs {
                    if needs_async(expr) {
                        let materialized = self
                            .executor
                            .materialize_expr_for_row(
                                expr,
                                row,
                                None,
                                Some(schema),
                                txn,
                                self.db_id,
                                seq,
                                self.search_path,
                                self.ctes,
                                &self.qctx,
                            )
                            .await?;
                        values.push(eval_typed_expr(&materialized, row, &self.qctx)?);
                    } else {
                        values.push(eval_typed_expr(expr, row, &self.qctx)?);
                    }
                }
                if has_srf {
                    self.expand_srf_row(values, exprs, &srf_indices, row, &mut projected)?;
                } else {
                    projected.push(Row::new(values));
                }
            }
            Ok(projected)
        } else if has_srf {
            let mut projected = Vec::with_capacity(rows.len());
            for row in &rows {
                let mut base_values = Vec::with_capacity(exprs.len());
                let mut srf_outputs: Vec<(usize, Vec<Value>)> = Vec::new();

                for (i, expr) in exprs.iter().enumerate() {
                    if let Some(&(_, kind)) = srf_indices.iter().find(|(idx, _)| *idx == i) {
                        let outputs = eval_srf(kind, expr, row, &self.qctx)?;
                        srf_outputs.push((i, outputs));
                        base_values.push(Value::Null); // placeholder
                    } else {
                        base_values.push(eval_typed_expr(expr, row, &self.qctx)?);
                    }
                }

                let max_len = srf_outputs
                    .iter()
                    .map(|(_, out)| out.len())
                    .max()
                    .unwrap_or(0);
                if max_len == 0 {
                    continue; // All SRFs returned empty → no rows (PostgreSQL behavior).
                }
                for i in 0..max_len {
                    let mut row_values = base_values.clone();
                    for (col_idx, out) in &srf_outputs {
                        row_values[*col_idx] = out.get(i).cloned().unwrap_or(Value::Null);
                    }
                    projected.push(Row::new(row_values));
                }
            }
            Ok(projected)
        } else {
            let mut projected = Vec::with_capacity(rows.len());
            for row in &rows {
                let mut values = Vec::with_capacity(exprs.len());
                for expr in exprs {
                    values.push(eval_typed_expr(expr, row, &self.qctx)?);
                }
                projected.push(Row::new(values));
            }
            Ok(projected)
        }
    }

    /// Expand a row containing SRF results into multiple output rows.
    fn expand_srf_row(
        &self,
        mut base_values: Vec<Value>,
        exprs: &[TypedExpr],
        srf_indices: &[(usize, SrfKind)],
        input: &Row,
        out: &mut Vec<Row>,
    ) -> Result<()> {
        let mut srf_outputs: Vec<(usize, Vec<Value>)> = Vec::new();
        for &(idx, kind) in srf_indices {
            let outputs = eval_srf(kind, &exprs[idx], input, &self.qctx)?;
            srf_outputs.push((idx, outputs));
            base_values[idx] = Value::Null; // overwrite the scalar-eval'd placeholder
        }
        let max_len = srf_outputs.iter().map(|(_, o)| o.len()).max().unwrap_or(0);
        if max_len == 0 {
            return Ok(()); // All SRFs empty → skip row.
        }
        for i in 0..max_len {
            let mut row_values = base_values.clone();
            for (col_idx, vals) in &srf_outputs {
                row_values[*col_idx] = vals.get(i).cloned().unwrap_or(Value::Null);
            }
            out.push(Row::new(row_values));
        }
        Ok(())
    }

    /// Execute an operator tree, handling CTE branching automatically.
    pub async fn run_operator_tree(
        &self,
        op: &mut BoxedOperator,
        txn: &mut Transaction,
        seq: &mut HashMap<String, i64>,
    ) -> Result<Vec<Row>> {
        if self.ctes.is_empty() {
            execute_operator_tree(
                self.executor,
                op,
                txn,
                self.executor.store(),
                self.db_id,
                self.search_path,
                seq,
            )
            .await
        } else {
            execute_operator_tree_with_ctes(
                self.executor,
                op,
                txn,
                self.executor.store(),
                self.db_id,
                self.search_path,
                seq,
                self.ctes,
            )
            .await
        }
    }
}

// ── Free functions for use outside ExprRuntime ──────────────────

/// Check if a TypedExpr needs async (per-row) materialization.
pub(super) fn needs_async(expr: &TypedExpr) -> bool {
    crate::sql::expr::classify::needs_async(expr)
}
