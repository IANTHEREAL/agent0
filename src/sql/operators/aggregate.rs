//! Hash aggregation with streaming input (#2555).
//!
//! `open()` consumes child rows one at a time and folds them into per-group
//! aggregator state, so live memory is O(groups × state), never O(input rows).
//!
//! # Memory accounting contract
//!
//! Retained state is charged to the statement memory scope as a **cheap,
//! conservative estimate measured from the real object after it is built**
//! (capacity deltas around `push`, `estimate_*` on existing values):
//!
//! - O(1) work per update; never rescan whole aggregate state to recompute.
//! - Accounting may over-estimate (rejecting slightly early is safe) and may
//!   under-count at most one in-flight transient **of single-row/value
//!   size**. The bound is on the transient's *size*, not just its count: any
//!   allocation that scales with retained state (joined strings, cloned
//!   value vectors, assembled JSON) must be admitted before it is
//!   materialized, with its size computed from the real retained parts
//!   (`Aggregator::result_consuming`). A row-sized transient cannot OOM a
//!   process that admitted all retained state; a state-sized one can.
//! - Over-charge is bounded the same way: **charges follow ownership**. When
//!   a charged row/page/state moves to its consumer (rows handed out of
//!   `next()`, state consumed by finalization), the source releases that
//!   share as part of the transfer. Transient double-charge during a handoff
//!   is bounded by the object being handed off, never by the accumulated
//!   result set — unbounded over-charge fails queries that fit the budget,
//!   which defeats the quota just as surely as under-counting OOMs it.
//! - **Mechanism, not discipline:** buffers that hold charged rows across
//!   method boundaries must use [`ChargedRowBuffer`], which owns the rows
//!   and their charge in one place so they cannot desynchronize. Raw
//!   `try_grow`/`try_shrink` pairs are allowed only when the grow and its
//!   matching shrink are both visible within a single function body (the
//!   per-group state and finalize accumulators in `open()` below).
//! - Byte-exactness against the allocator is explicitly a **non-goal**; do
//!   not add size predictors that shadow formatter/display internals.

use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use super::charged_rows::{ChargedBuf, ChargedEntry, ChargedRowBuffer};
use super::key_encoding::{encode_value_key, encode_values_key};
use super::{BoxedOperator, ExecutionContext, PhysicalOperator};
use crate::model::{ColumnDef, DataType, Row, TableSchema, Value};
use crate::pool::{try_grow_statement_memory_scope, try_shrink_statement_memory_scope};
use crate::sql::analyzer::types::{TypedExpr, TypedOrderByExpr};
use crate::sql::expr::classify::needs_async;
use crate::sql::expr::compare_order_by_values;
use crate::sql::expr::operators::sort_by_fallible;
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::memory::{estimate_key_size, estimate_value_size, estimate_values_payload_size};
use crate::sql::{AggregateStateDelta, Aggregator};

#[derive(Debug, Clone)]
pub struct AggregateExpr {
    pub func_name: String,
    pub arg: Option<TypedExpr>,
    pub distinct: bool,
    /// The raw delimiter expression for `string_agg`, evaluated per-row.
    pub delimiter: Option<TypedExpr>,
    pub filter: Option<TypedExpr>,
    pub order_by: Vec<TypedOrderByExpr>,
}

#[derive(Debug)]
pub struct HashAggregateOperator {
    child: BoxedOperator,
    group_by_exprs: Vec<TypedExpr>,
    aggregate_exprs: Vec<AggregateExpr>,
    output_schema: TableSchema,
    result_rows: ChargedRowBuffer,
    opened: bool,
    child_closed: bool,
}

impl HashAggregateOperator {
    pub fn new(
        child: BoxedOperator,
        group_by_exprs: Vec<TypedExpr>,
        aggregate_exprs: Vec<AggregateExpr>,
        group_by_names: Vec<String>,
        group_by_types: Vec<DataType>,
        aggregate_names: Vec<String>,
        aggregate_types: Vec<DataType>,
    ) -> Self {
        let mut columns = Vec::new();

        for (name, dt) in group_by_names.iter().zip(group_by_types.iter()) {
            columns.push(ColumnDef::new(name.clone(), dt.clone(), true));
        }

        for (name, dt) in aggregate_names.iter().zip(aggregate_types.iter()) {
            columns.push(ColumnDef::new(name.clone(), dt.clone(), true));
        }

        let output_schema = TableSchema::virtual_table("aggregate", columns);

        Self {
            child,
            group_by_exprs,
            aggregate_exprs,
            output_schema,
            result_rows: ChargedRowBuffer::new(),
            opened: false,
            child_closed: true,
        }
    }

    fn create_aggregator(agg_expr: &AggregateExpr, return_type: &DataType) -> Result<Aggregator> {
        if agg_expr.func_name == "STRING_AGG" {
            return Ok(Aggregator::new_string_agg());
        }
        Aggregator::new(&agg_expr.func_name, Some(return_type.clone()))
    }

    /// Fold one value into an aggregator. Retained growth is admitted inside
    /// `update_charged` (before allocation) against the statement scope;
    /// shrinks are released here. The returned delta is recorded into the
    /// group's charge accumulator by the caller — its grows are already in
    /// the scope and must not be re-charged.
    fn apply_aggregate_update(
        agg: &mut Aggregator,
        val: &Value,
        delimiter: &str,
        is_string_agg: bool,
    ) -> Result<AggregateStateDelta> {
        let charge = &mut |bytes: usize| {
            try_grow_statement_memory_scope("operators.hash_aggregate.aggregate_state", bytes)
                .map_err(anyhow::Error::from)
        };
        let delta = if is_string_agg {
            agg.update_string_agg_charged(val, delimiter, charge)?
        } else {
            agg.update_charged(val, charge)?
        };
        if delta.shrink_bytes > 0 {
            try_shrink_statement_memory_scope(delta.shrink_bytes);
        }
        Ok(delta)
    }

    fn record_charged_delta(charged_bytes: &mut usize, delta: AggregateStateDelta) {
        if delta.grow_bytes > 0 {
            *charged_bytes = charged_bytes.saturating_add(delta.grow_bytes);
        }
        if delta.shrink_bytes > 0 {
            *charged_bytes = charged_bytes.saturating_sub(delta.shrink_bytes);
        }
    }

    fn push_result_row(&mut self, row: Row) -> Result<()> {
        self.result_rows
            .push("operators.hash_aggregate.result_rows", row)
    }
}

/// Ordered-aggregate buffer entry: `(ORDER BY keys, value, delimiter)`.
/// Slot bytes are accounted by the owning [`ChargedBuf`]; this reports heap
/// payload only.
impl ChargedEntry for (Vec<Value>, Value, String) {
    fn charged_size(&self) -> usize {
        estimate_values_payload_size(&self.0) + estimate_value_size(&self.1) + self.2.len()
    }
    fn hollow() -> Self {
        (Vec::new(), Value::Null, String::new())
    }
}

#[async_trait]
impl PhysicalOperator for HashAggregateOperator {
    fn schema(&self) -> &TableSchema {
        &self.output_schema
    }

    async fn open(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.result_rows.reset();
        self.child.open(ctx).await?;
        self.child_closed = false;

        struct GroupState {
            group_values: Vec<Value>,
            aggregators: Vec<Aggregator>,
            seen_distinct: Vec<HashSet<Vec<u8>>>,
            ordered_agg_buffers: Vec<Option<ChargedBuf<(Vec<Value>, Value, String)>>>,
            charged_bytes: usize,
        }

        let mut groups: HashMap<Vec<u8>, GroupState> = HashMap::new();

        while let Some(row) = self.child.next(ctx).await? {
            let mut group_key_values = Vec::new();
            for expr in &self.group_by_exprs {
                let val = eval_typed_expr(expr, &row, ctx.query_ctx)?;
                group_key_values.push(val);
            }

            let key_bytes = encode_values_key(&group_key_values);

            if !groups.contains_key(&key_bytes) {
                let aggregators: Vec<Aggregator> = self
                    .aggregate_exprs
                    .iter()
                    .enumerate()
                    .map(|(i, agg_expr)| {
                        let rt =
                            &self.output_schema.columns[self.group_by_exprs.len() + i].data_type;
                        Self::create_aggregator(agg_expr, rt)
                    })
                    .collect::<Result<Vec<_>>>()?;
                let seen_distinct: Vec<HashSet<Vec<u8>>> = (0..self.aggregate_exprs.len())
                    .map(|_| HashSet::new())
                    .collect();
                let ordered_agg_buffers: Vec<Option<ChargedBuf<(Vec<Value>, Value, String)>>> =
                    self.aggregate_exprs
                        .iter()
                        .map(|agg_expr| {
                            if !agg_expr.order_by.is_empty() {
                                Some(ChargedBuf::new())
                            } else {
                                None
                            }
                        })
                        .collect();
                // Two table slots per entry are prepaid so every future
                // bucket-array doubling of `groups` is admitted before it
                // allocates (same amortization as the distinct sets below).
                let group_overhead_bytes = 2 * (std::mem::size_of::<(Vec<u8>, GroupState)>() + 1)
                    + key_bytes.len()
                    + estimate_values_payload_size(&group_key_values)
                    + std::mem::size_of_val(aggregators.as_slice())
                    + std::mem::size_of_val(seen_distinct.as_slice())
                    + std::mem::size_of_val(ordered_agg_buffers.as_slice());
                try_grow_statement_memory_scope(
                    "operators.hash_aggregate.groups",
                    group_overhead_bytes,
                )?;
                groups.insert(
                    key_bytes.clone(),
                    GroupState {
                        group_values: group_key_values.clone(),
                        aggregators,
                        seen_distinct,
                        ordered_agg_buffers,
                        charged_bytes: group_overhead_bytes,
                    },
                );
            }

            let state = groups
                .get_mut(&key_bytes)
                .ok_or_else(|| anyhow!("Aggregate group state missing"))?;

            for (i, agg_expr) in self.aggregate_exprs.iter().enumerate() {
                if let Some(ref filter_expr) = agg_expr.filter {
                    let filter_val = if needs_async(filter_expr) {
                        let mat = ctx
                            .executor
                            .materialize_expr_for_row(
                                filter_expr,
                                &row,
                                ctx.outer_row.as_ref(),
                                Some(self.child.schema()),
                                ctx.txn,
                                ctx.db_id,
                                ctx.sequence_values,
                                ctx.search_path,
                                ctx.cte_tables,
                                ctx.query_ctx,
                            )
                            .await?;
                        eval_typed_expr(&mat, &row, ctx.query_ctx)?
                    } else {
                        eval_typed_expr(filter_expr, &row, ctx.query_ctx)?
                    };
                    if !matches!(filter_val, Value::Boolean(true)) {
                        continue;
                    }
                }

                let val = if let Some(arg) = &agg_expr.arg {
                    if needs_async(arg) {
                        let mat = ctx
                            .executor
                            .materialize_expr_for_row(
                                arg,
                                &row,
                                ctx.outer_row.as_ref(),
                                Some(self.child.schema()),
                                ctx.txn,
                                ctx.db_id,
                                ctx.sequence_values,
                                ctx.search_path,
                                ctx.cte_tables,
                                ctx.query_ctx,
                            )
                            .await?;
                        eval_typed_expr(&mat, &row, ctx.query_ctx)?
                    } else {
                        eval_typed_expr(arg, &row, ctx.query_ctx)?
                    }
                } else {
                    Value::Int32(1)
                };

                if agg_expr.distinct {
                    let val_bytes = encode_value_key(&val);
                    if state.seen_distinct[i].contains(&val_bytes) {
                        continue;
                    }
                    // Admit before the insert retains the key or doubles the
                    // table. The per-entry charge prepays two slots, which
                    // amortizes every future bucket-array doubling (a
                    // doubling at len N allocates N slots; the N entries
                    // already prepaid 2N), so growth never allocates ahead
                    // of admission. The first allocation is under-prepaid by
                    // a constant handful of slots — bounded, within
                    // contract.
                    let distinct_entry_bytes = estimate_key_size(val_bytes.as_slice())
                        .saturating_add(2 * (std::mem::size_of::<Vec<u8>>() + 1));
                    try_grow_statement_memory_scope(
                        "operators.hash_aggregate.seen_distinct",
                        distinct_entry_bytes,
                    )?;
                    state.seen_distinct[i].insert(val_bytes);
                    state.charged_bytes = state.charged_bytes.saturating_add(distinct_entry_bytes);
                }

                // Evaluate per-row delimiter for string_agg.
                let row_delimiter = if agg_expr.func_name == "STRING_AGG" {
                    if let Some(ref delim_expr) = agg_expr.delimiter {
                        let dv = if needs_async(delim_expr) {
                            let mat = ctx
                                .executor
                                .materialize_expr_for_row(
                                    delim_expr,
                                    &row,
                                    ctx.outer_row.as_ref(),
                                    Some(self.child.schema()),
                                    ctx.txn,
                                    ctx.db_id,
                                    ctx.sequence_values,
                                    ctx.search_path,
                                    ctx.cte_tables,
                                    ctx.query_ctx,
                                )
                                .await?;
                            eval_typed_expr(&mat, &row, ctx.query_ctx)?
                        } else {
                            eval_typed_expr(delim_expr, &row, ctx.query_ctx)?
                        };
                        match dv {
                            Value::Text(s) => s,
                            Value::Null => String::new(),
                            Value::Bytes(b) => String::from_utf8_lossy(&b).into_owned(),
                            other => other.to_string(),
                        }
                    } else {
                        ",".to_string()
                    }
                } else {
                    String::new()
                };

                if let Some(buf) = state.ordered_agg_buffers[i].as_mut() {
                    let mut keys = Vec::with_capacity(agg_expr.order_by.len());
                    for o in &agg_expr.order_by {
                        let key = if needs_async(&o.expr) {
                            let mat = ctx
                                .executor
                                .materialize_expr_for_row(
                                    &o.expr,
                                    &row,
                                    ctx.outer_row.as_ref(),
                                    Some(self.child.schema()),
                                    ctx.txn,
                                    ctx.db_id,
                                    ctx.sequence_values,
                                    ctx.search_path,
                                    ctx.cte_tables,
                                    ctx.query_ctx,
                                )
                                .await?;
                            eval_typed_expr(&mat, &row, ctx.query_ctx)?
                        } else {
                            eval_typed_expr(&o.expr, &row, ctx.query_ctx)?
                        };
                        keys.push(key);
                    }
                    // The buffer owns its payload+slot charge (admitted
                    // before allocation inside push) and releases it as
                    // entries are drained or when it is dropped — it never
                    // enters the group accumulator.
                    buf.push(
                        "operators.hash_aggregate.ordered_buffer",
                        (keys, val, row_delimiter),
                    )?;
                } else {
                    let delta = Self::apply_aggregate_update(
                        &mut state.aggregators[i],
                        &val,
                        &row_delimiter,
                        agg_expr.func_name == "STRING_AGG",
                    )?;
                    Self::record_charged_delta(&mut state.charged_bytes, delta);
                }
            }
        }

        self.child.close(ctx).await?;
        self.child_closed = true;

        if groups.is_empty() && self.group_by_exprs.is_empty() {
            let mut values = Vec::new();
            let mut finalize_charged = 0usize;
            for (i, agg_expr) in self.aggregate_exprs.iter().enumerate() {
                let rt = &self.output_schema.columns[i].data_type;
                let mut agg = Self::create_aggregator(agg_expr, rt)?;
                values.push(agg.result_consuming(&mut |bytes| {
                    try_grow_statement_memory_scope("operators.hash_aggregate.finalize", bytes)?;
                    finalize_charged = finalize_charged.saturating_add(bytes);
                    Ok(())
                })?);
            }
            let out_row = Row::new(values);
            self.push_result_row(out_row)?;
            try_shrink_statement_memory_scope(finalize_charged);
        } else {
            // In-contract (see the memory-accounting contract on
            // `TenantMemoryAccountant` in pool.rs): consuming `groups` by
            // IntoIter keeps the whole hashbrown bucket array resident until
            // this loop ends, while each group's `charged_bytes` (incl. the
            // 2-slot bucket prepay) is released per-group below. The
            // released-early window is the bucket array only — O(group count),
            // ~256 B/group, already admitted at build time and bounded by a
            // single map. That is a release-timing artifact within the
            // constant-factor headroom, not an O(input) under-count, so it is
            // deliberately not point-fixed here; routing the groups map (and
            // `seen_distinct`) through a charged collection is the successor-
            // epic class fix.
            for (_, state) in groups {
                let GroupState {
                    group_values,
                    aggregators,
                    mut ordered_agg_buffers,
                    mut charged_bytes,
                    ..
                } = state;
                let mut values = group_values;
                // Finalization charges (result strings/arrays/JSON assembled by
                // `result_consuming`) are admitted here and released right after
                // `push_result_row` re-charges the retained row, so the
                // double-charge window is one group, not the whole result set.
                let mut finalize_charged = 0usize;
                for (i, mut agg) in aggregators.into_iter().enumerate() {
                    if let Some(mut buf) = ordered_agg_buffers.get_mut(i).and_then(Option::take) {
                        let order_by = &self.aggregate_exprs[i].order_by;
                        sort_by_fallible(buf.as_mut_slice(), |(keys_a, _, _), (keys_b, _, _)| {
                            for (key_idx, order_expr) in order_by.iter().enumerate() {
                                let asc = order_expr.asc;
                                let nulls_first = order_expr.nulls_first;
                                let ord = compare_order_by_values(
                                    &keys_a[key_idx],
                                    &keys_b[key_idx],
                                    asc,
                                    nulls_first,
                                )?;
                                if ord != std::cmp::Ordering::Equal {
                                    return Ok(ord);
                                }
                            }
                            Ok(std::cmp::Ordering::Equal)
                        })?;
                        let is_string_agg = self.aggregate_exprs[i].func_name == "STRING_AGG";
                        // take_next releases each entry's payload share as it
                        // is consumed (the update re-admits what the
                        // aggregator retains); dropping the buffer right
                        // after releases its slot charge with the freed
                        // allocation, before finalization needs quota.
                        while let Some((_, sorted_value, delim)) = buf.take_next() {
                            let delta = Self::apply_aggregate_update(
                                &mut agg,
                                &sorted_value,
                                &delim,
                                is_string_agg,
                            )?;
                            Self::record_charged_delta(&mut charged_bytes, delta);
                        }
                        drop(buf);
                    }
                    values.push(agg.result_consuming(&mut |bytes| {
                        try_grow_statement_memory_scope(
                            "operators.hash_aggregate.finalize",
                            bytes,
                        )?;
                        finalize_charged = finalize_charged.saturating_add(bytes);
                        Ok(())
                    })?);
                }
                // The aggregators consumed their retained state in
                // result_consuming (and the ordered buffers were drained by
                // the replay), so release the state charge before charging
                // the result row — otherwise state + finalize + row are all
                // charged at once, demanding ~3x quota for a single huge
                // group whose real peak is the result plus its in-flight
                // copy.
                drop(ordered_agg_buffers);
                try_shrink_statement_memory_scope(charged_bytes);
                let out_row = Row::new(values);
                self.push_result_row(out_row)?;
                try_shrink_statement_memory_scope(finalize_charged);
            }
        }

        self.opened = true;
        Ok(())
    }

    async fn next(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>> {
        if !self.opened {
            return Err(anyhow!("Operator not opened"));
        }
        // ChargedRowBuffer hands the row out by move and releases its charge
        // share with the transfer (the consumer charges its own retention).
        Ok(self.result_rows.take_next())
    }

    async fn close(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        let child_close_result = if self.child_closed {
            Ok(())
        } else {
            let result = self.child.close(ctx).await;
            self.child_closed = true;
            result
        };
        self.result_rows.reset();
        self.opened = false;
        child_close_result
    }

    #[cfg(test)]
    fn name(&self) -> &'static str {
        "HashAggregate"
    }

    #[cfg(test)]
    fn explain_info(&self) -> Option<String> {
        let group_cols: Vec<String> = self
            .group_by_exprs
            .iter()
            .map(|e| format!("{:?}", e))
            .collect();
        let agg_funcs: Vec<String> = self
            .aggregate_exprs
            .iter()
            .map(|a| a.func_name.clone())
            .collect();

        if group_cols.is_empty() {
            Some(format!("aggs=[{}]", agg_funcs.join(", ")))
        } else {
            Some(format!(
                "group_by=[{}], aggs=[{}]",
                group_cols.join(", "),
                agg_funcs.join(", ")
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::analyzer::types::BinaryOp as TypedBinaryOp;
    use crate::sql::analyzer::types::TypedExprKind;
    use crate::sql::operators::scan::TableScanOperator;

    fn test_schema() -> TableSchema {
        TableSchema::new(
            "sales".to_string(),
            1,
            vec![
                ColumnDef::new("category", DataType::Text, false),
                ColumnDef::new("amount", DataType::Int32, false),
            ],
            vec![],
        )
    }

    fn category_ref() -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: 0,
                column_name: "category".to_string(),
            },
            data_type: DataType::Text,
        }
    }

    fn amount_ref() -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: 1,
                column_name: "amount".to_string(),
            },
            data_type: DataType::Int32,
        }
    }

    #[test]
    fn test_hash_aggregate_creation() {
        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let group_by_exprs = vec![category_ref()];
        let aggregate_exprs = vec![AggregateExpr {
            func_name: "SUM".to_string(),
            arg: Some(amount_ref()),
            distinct: false,
            delimiter: None,
            filter: None,
            order_by: vec![],
        }];

        let op = HashAggregateOperator::new(
            child,
            group_by_exprs,
            aggregate_exprs,
            vec!["category".to_string()],
            vec![DataType::Text],
            vec!["sum_amount".to_string()],
            vec![DataType::Int64],
        );

        assert_eq!(op.name(), "HashAggregate");
        assert_eq!(op.schema().columns.len(), 2);
        assert_eq!(op.schema().columns[0].name, "category");
        assert_eq!(op.schema().columns[1].name, "sum_amount");
    }

    #[test]
    fn test_hash_aggregate_explain_info() {
        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let group_by_exprs = vec![category_ref()];
        let aggregate_exprs = vec![
            AggregateExpr {
                func_name: "COUNT".to_string(),
                arg: None,
                distinct: false,
                delimiter: None,
                filter: None,
                order_by: vec![],
            },
            AggregateExpr {
                func_name: "SUM".to_string(),
                arg: Some(amount_ref()),
                distinct: false,
                delimiter: None,
                filter: None,
                order_by: vec![],
            },
        ];

        let op = HashAggregateOperator::new(
            child,
            group_by_exprs,
            aggregate_exprs,
            vec!["category".to_string()],
            vec![DataType::Text],
            vec!["count".to_string(), "sum_amount".to_string()],
            vec![DataType::Int64, DataType::Int64],
        );

        let info = op.explain_info().unwrap();
        assert!(info.contains("group_by="));
        assert!(info.contains("category"));
        assert!(info.contains("COUNT"));
        assert!(info.contains("SUM"));
    }

    #[test]
    fn test_hash_aggregate_no_group_by() {
        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let aggregate_exprs = vec![AggregateExpr {
            func_name: "COUNT".to_string(),
            arg: None,
            distinct: false,
            delimiter: None,
            filter: None,
            order_by: vec![],
        }];

        let op = HashAggregateOperator::new(
            child,
            vec![],
            aggregate_exprs,
            vec![],
            vec![],
            vec!["count".to_string()],
            vec![DataType::Int64],
        );

        let info = op.explain_info().unwrap();
        assert!(info.contains("aggs="));
        assert!(!info.contains("group_by="));
    }

    #[test]
    fn test_hash_aggregate_multiple_group_columns() {
        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let group_by_exprs = vec![category_ref(), amount_ref()];
        let aggregate_exprs = vec![AggregateExpr {
            func_name: "COUNT".to_string(),
            arg: None,
            distinct: false,
            delimiter: None,
            filter: None,
            order_by: vec![],
        }];

        let op = HashAggregateOperator::new(
            child,
            group_by_exprs,
            aggregate_exprs,
            vec!["category".to_string(), "amount".to_string()],
            vec![DataType::Text, DataType::Int32],
            vec!["count".to_string()],
            vec![DataType::Int64],
        );

        assert_eq!(op.schema().columns.len(), 3);
        assert_eq!(op.schema().columns[0].name, "category");
        assert_eq!(op.schema().columns[1].name, "amount");
        assert_eq!(op.schema().columns[2].name, "count");

        let info = op.explain_info().unwrap();
        assert!(info.contains("category"));
        assert!(info.contains("amount"));
    }

    #[test]
    fn test_hash_aggregate_string_agg_delimiter() {
        let agg_expr = AggregateExpr {
            func_name: "STRING_AGG".to_string(),
            arg: Some(category_ref()),
            distinct: false,
            delimiter: Some(TypedExpr {
                kind: crate::sql::analyzer::types::TypedExprKind::Constant(Value::Text(
                    ", ".to_string(),
                )),
                data_type: DataType::Text,
            }),
            filter: None,
            order_by: vec![],
        };

        let aggregator =
            HashAggregateOperator::create_aggregator(&agg_expr, &DataType::Text).unwrap();
        assert_eq!(aggregator.result().unwrap(), Value::Null);
    }

    #[test]
    fn test_hash_aggregate_with_filter_creation() {
        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let filter_expr = TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(amount_ref()),
                op: TypedBinaryOp::Gt,
                right: Box::new(TypedExpr {
                    kind: TypedExprKind::Constant(Value::Int64(100)),
                    data_type: DataType::Int64,
                }),
            },
            data_type: DataType::Boolean,
        };

        let aggregate_exprs = vec![AggregateExpr {
            func_name: "SUM".to_string(),
            arg: Some(amount_ref()),
            distinct: false,
            delimiter: None,
            filter: Some(filter_expr),
            order_by: vec![],
        }];

        let op = HashAggregateOperator::new(
            child,
            vec![],
            aggregate_exprs.clone(),
            vec![],
            vec![],
            vec!["filtered_sum".to_string()],
            vec![DataType::Int64],
        );

        assert_eq!(op.name(), "HashAggregate");
        assert!(op.aggregate_exprs[0].filter.is_some());
    }
}
