//! Unified expression evaluation boundary for the analyzed SELECT executor.
//!
//! `ExprRuntime` wraps the three-phase expression lifecycle:
//! - **Phase 1 (batch):** pre-materialize uncorrelated subqueries / sequences
//! - **Phase 2 (sync):** pure `eval_typed_expr` (hot path for operators)
//! - **Phase 3 (per-row async):** materialize correlated subqueries / catalog funcs, then eval
//!
//! The executor calls `ExprRuntime` methods instead of hand-writing per-row loops.
//! Operators are NOT affected — they continue calling `eval_typed_expr` directly.

use crate::sql::analyzer::types::{AnalyzedProjection, JoinType, TypedExpr, TypedOrderByExpr};
use crate::sql::executor::core::Executor;
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::operators::{
    detect_srf, eval_srf, execute_operator_tree, execute_operator_tree_with_ctes, BoxedOperator,
    SrfKind,
};
use crate::sql::query_context::QueryContext;
use crate::types::{Row, TableSchema, Value};

use anyhow::{anyhow, Result};
use std::collections::HashMap;
use tikv_client::Transaction;

use super::subquery::split_where_for_async;
use crate::sql::expr::classify::{has_catalog_dependent_function, has_unresolved_subquery};

// ── Supporting types ──────────────────────────────────────────────

pub(super) struct PreparedWhere {
    pub sync_part: Option<TypedExpr>,
    pub async_part: Option<TypedExpr>,
}

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

    // ── Phase 1: Batch pre-materialization ──────────────────────

    /// Pre-materialize uncorrelated subqueries/sequences in a single expression.
    pub async fn pre_materialize(
        &self,
        expr: &TypedExpr,
        txn: &mut Transaction,
        seq: &mut HashMap<String, i64>,
    ) -> Result<TypedExpr> {
        self.executor
            .pre_materialize_async_exprs(expr, txn, self.db_id, seq, self.search_path, self.ctes)
            .await
    }

    /// Pre-materialize WHERE clause and split into sync/async parts.
    pub async fn prepare_where(
        &self,
        where_clause: &Option<TypedExpr>,
        txn: &mut Transaction,
        seq: &mut HashMap<String, i64>,
    ) -> Result<PreparedWhere> {
        let Some(ref w) = where_clause else {
            return Ok(PreparedWhere {
                sync_part: None,
                async_part: None,
            });
        };

        let materialized = self.pre_materialize(w, txn, seq).await?;

        let (sync_part, async_part) = if has_catalog_dependent_function(&materialized) {
            (None, Some(materialized))
        } else if has_unresolved_subquery(&materialized) {
            split_where_for_async(&materialized)
        } else {
            (Some(materialized), None)
        };

        Ok(PreparedWhere {
            sync_part,
            async_part,
        })
    }

    /// Pre-materialize ORDER BY expressions and classify as inline or deferred.
    ///
    /// Returns `(inline_order_by, deferred_order_by)`:
    /// - `inline`: safe for SortOperator (no async exprs after materialization)
    /// - `deferred`: contains async exprs, must sort after projection
    pub async fn prepare_order_by(
        &self,
        order_by: &[TypedOrderByExpr],
        txn: &mut Transaction,
        seq: &mut HashMap<String, i64>,
    ) -> Result<(Vec<TypedOrderByExpr>, Option<Vec<TypedOrderByExpr>>)> {
        if order_by.is_empty() {
            return Ok((vec![], None));
        }

        let mut materialized = Vec::with_capacity(order_by.len());
        for ob in order_by {
            let m = self.pre_materialize(&ob.expr, txn, seq).await?;
            materialized.push(TypedOrderByExpr {
                expr: m,
                asc: ob.asc,
                nulls_first: ob.nulls_first,
            });
        }

        let needs_defer = materialized.iter().any(|o| needs_async(&o.expr));
        if needs_defer {
            Ok((vec![], Some(materialized)))
        } else {
            Ok((materialized, None))
        }
    }

    /// Pre-materialize all projection expressions.
    pub async fn pre_materialize_projections(
        &self,
        projection: &[AnalyzedProjection],
        txn: &mut Transaction,
        seq: &mut HashMap<String, i64>,
    ) -> Result<Vec<TypedExpr>> {
        let mut exprs = Vec::with_capacity(projection.len());
        for p in projection {
            exprs.push(self.pre_materialize(&p.expr, txn, seq).await?);
        }
        Ok(exprs)
    }

    /// Fully materialize async expressions, replacing them with sync equivalents.
    ///
    /// Used in tableless path where expressions must be pure before passing to
    /// `ProjectOperator`.
    pub async fn materialize_exprs_for_row(
        &self,
        exprs: Vec<TypedExpr>,
        row: &Row,
        schema: &TableSchema,
        txn: &mut Transaction,
        seq: &mut HashMap<String, i64>,
    ) -> Result<Vec<TypedExpr>> {
        let mut result = Vec::with_capacity(exprs.len());
        for expr in exprs {
            if needs_async(&expr) {
                result.push(
                    self.executor
                        .materialize_expr_for_row(
                            &expr,
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
                        .await?,
                );
            } else {
                result.push(expr);
            }
        }
        Ok(result)
    }

    // ── Phase 2: Sync evaluation ────────────────────────────────

    /// Pure sync evaluation (hot path). Wraps `eval_typed_expr`.
    ///
    /// Will be wired when mod.rs hand-written loops are replaced.
    #[inline]
    #[allow(dead_code)]
    pub fn eval(&self, expr: &TypedExpr, row: &Row) -> Result<Value> {
        eval_typed_expr(expr, row, &self.qctx)
    }

    // ── Phase 3: Per-row async evaluation ───────────────────────

    /// Materialize correlated subqueries/catalog funcs for a row, then evaluate.
    ///
    /// Will be wired when mod.rs hand-written loops are replaced.
    #[allow(dead_code)]
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

    /// Async WHERE post-filter: evaluate async predicate per-row.
    /// If `pred` is None, returns rows unchanged.
    pub async fn filter_async(
        &self,
        rows: Vec<Row>,
        pred: Option<&TypedExpr>,
        schema: &TableSchema,
        txn: &mut Transaction,
        seq: &mut HashMap<String, i64>,
    ) -> Result<Vec<Row>> {
        let Some(pred) = pred else {
            return Ok(rows);
        };
        let mut filtered = Vec::new();
        for row in rows {
            let materialized = self
                .executor
                .materialize_expr_for_row(
                    pred,
                    &row,
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
            let val = eval_typed_expr(&materialized, &row, &self.qctx)?;
            if matches!(val, Value::Boolean(true)) {
                filtered.push(row);
            }
        }
        Ok(filtered)
    }

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

        let any_async = exprs.iter().any(|e| needs_async(e));
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

/// Execute an async nested-loop join with per-row ON condition materialization.
///
/// Used when JOIN ON contains correlated subqueries or catalog-dependent
/// functions that can't be pushed into the `NestedLoopJoinOperator`.
#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_async_nested_loop_join(
    executor: &Executor,
    join_type: JoinType,
    left_rows: &[Row],
    right_rows: &[Row],
    on_expr: &TypedExpr,
    output_schema: &TableSchema,
    left_col_count: usize,
    right_col_count: usize,
    txn: &mut Transaction,
    db_id: u64,
    seq: &mut HashMap<String, i64>,
    search_path: &[String],
    ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
) -> Result<Vec<Row>> {
    let qc = QueryContext::from_task_locals();
    let null_left = Row::new(vec![Value::Null; left_col_count]);
    let null_right = Row::new(vec![Value::Null; right_col_count]);

    /// Combine left + right row into a single joined row.
    fn combine(left: &Row, right: &Row) -> Row {
        let mut values = left.values.clone();
        values.extend(right.values.clone());
        Row::new(values)
    }

    /// Evaluate the ON condition for a combined row, returning match result.
    async fn eval_on(
        executor: &Executor,
        on_expr: &TypedExpr,
        combined: &Row,
        schema: &TableSchema,
        txn: &mut Transaction,
        db_id: u64,
        seq: &mut HashMap<String, i64>,
        search_path: &[String],
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
        qc: &QueryContext,
    ) -> Result<bool> {
        let materialized = executor
            .materialize_expr_for_row(
                on_expr,
                combined,
                None,
                Some(schema),
                txn,
                db_id,
                seq,
                search_path,
                ctes,
                qc,
            )
            .await?;
        let v = eval_typed_expr(&materialized, combined, qc)?;
        match v {
            Value::Boolean(b) => Ok(b),
            Value::Null => Ok(false),
            _ => Err(anyhow!("JOIN ON condition must evaluate to boolean")),
        }
    }

    let mut result = Vec::new();

    match join_type {
        JoinType::Inner => {
            for l in left_rows {
                for r in right_rows {
                    let combined = combine(l, r);
                    if eval_on(
                        executor,
                        on_expr,
                        &combined,
                        output_schema,
                        txn,
                        db_id,
                        seq,
                        search_path,
                        ctes,
                        &qc,
                    )
                    .await?
                    {
                        result.push(combined);
                    }
                }
            }
        }
        JoinType::Left => {
            for l in left_rows {
                let mut matched_any = false;
                for r in right_rows {
                    let combined = combine(l, r);
                    if eval_on(
                        executor,
                        on_expr,
                        &combined,
                        output_schema,
                        txn,
                        db_id,
                        seq,
                        search_path,
                        ctes,
                        &qc,
                    )
                    .await?
                    {
                        result.push(combined);
                        matched_any = true;
                    }
                }
                if !matched_any {
                    result.push(combine(l, &null_right));
                }
            }
        }
        JoinType::Right => {
            for r in right_rows {
                let mut matched_any = false;
                for l in left_rows {
                    let combined = combine(l, r);
                    if eval_on(
                        executor,
                        on_expr,
                        &combined,
                        output_schema,
                        txn,
                        db_id,
                        seq,
                        search_path,
                        ctes,
                        &qc,
                    )
                    .await?
                    {
                        result.push(combined);
                        matched_any = true;
                    }
                }
                if !matched_any {
                    result.push(combine(&null_left, r));
                }
            }
        }
        JoinType::Full => {
            let mut right_matched = vec![false; right_rows.len()];
            for l in left_rows {
                let mut left_matched = false;
                for (i, r) in right_rows.iter().enumerate() {
                    let combined = combine(l, r);
                    if eval_on(
                        executor,
                        on_expr,
                        &combined,
                        output_schema,
                        txn,
                        db_id,
                        seq,
                        search_path,
                        ctes,
                        &qc,
                    )
                    .await?
                    {
                        result.push(combined);
                        left_matched = true;
                        right_matched[i] = true;
                    }
                }
                if !left_matched {
                    result.push(combine(l, &null_right));
                }
            }
            for (i, r) in right_rows.iter().enumerate() {
                if !right_matched[i] {
                    result.push(combine(&null_left, r));
                }
            }
        }
        JoinType::Cross => {
            for l in left_rows {
                for r in right_rows {
                    result.push(combine(l, r));
                }
            }
        }
    }

    Ok(result)
}

/// Build a join output schema from left and right schemas.
pub(super) fn build_join_output_schema(
    left_schema: &TableSchema,
    right_schema: &TableSchema,
) -> TableSchema {
    let mut out_cols: Vec<crate::types::ColumnDef> = Vec::new();
    for col in &left_schema.columns {
        out_cols.push(crate::types::ColumnDef {
            name: col.name.clone(),
            data_type: col.data_type.clone(),
            nullable: true,
            primary_key: false,
            unique: false,
            is_serial: false,
            default_expr: None,
        });
    }
    for col in &right_schema.columns {
        out_cols.push(crate::types::ColumnDef {
            name: col.name.clone(),
            data_type: col.data_type.clone(),
            nullable: true,
            primary_key: false,
            unique: false,
            is_serial: false,
            default_expr: None,
        });
    }
    TableSchema {
        name: "join".to_string(),
        table_id: 0,
        columns: out_cols,
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        from_alias: None,
    }
}
