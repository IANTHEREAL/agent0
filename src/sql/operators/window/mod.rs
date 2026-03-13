//! Window function operator for Volcano-style execution
//!
//! This operator computes window functions over partitioned and ordered data.
//! Sub-modules contain the individual window function implementations:
//! - `ranking`: row_number, rank, dense_rank, ntile, percent_rank, cume_dist
//! - `aggregates`: sum, count, avg, min, max (window variants)
//! - `access`: lag, lead, first_value, last_value, nth_value

mod access;
mod aggregates;
mod ranking;

#[cfg(test)]
mod tests;

use std::collections::HashMap;

use crate::sql::analyzer::types::{
    TypedExpr, TypedOrderByExpr, WindowFrame, WindowFrameBound, WindowFrameUnits,
};
use crate::sql::error::SqlError;
use anyhow::{anyhow, Result};
use async_trait::async_trait;

use super::{collect_all, BoxedOperator, ExecutionContext, PhysicalOperator};
use crate::model::{ColumnDef, DataType, Row, TableSchema, Value};
use crate::sql::expr::compare_order_by_values;
use crate::sql::expr::operators::sort_by_fallible;
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::operators::key_encoding::encode_values_key;
use crate::sql::query_context::QueryContext;

/// Information about a single window function in the projection.
#[derive(Debug, Clone)]
pub struct WindowFunctionExpr {
    /// Name of the window function (e.g., "row_number", "sum", "lag")
    pub func_name: String,
    /// Argument expression (if any)
    pub arg_expr: Option<TypedExpr>,
    /// PARTITION BY expressions
    pub partition_by: Vec<TypedExpr>,
    /// ORDER BY expressions within the window
    pub order_by: Vec<TypedOrderByExpr>,
    /// Offset expression for LAG/LEAD
    pub offset_expr: Option<TypedExpr>,
    /// Default value expression for LAG/LEAD
    pub default_value_expr: Option<TypedExpr>,
    /// Window frame specification
    pub window_frame: Option<WindowFrame>,
    /// FILTER (WHERE ...) clause for window aggregate functions
    pub filter_expr: Option<TypedExpr>,
    /// Output column name
    pub output_name: String,
    /// Output data type
    pub output_type: DataType,
}

/// Window function operator that computes window functions over input rows.
///
/// This operator materializes all input rows, partitions them, and computes
/// window function values for each row.
#[derive(Debug)]
pub struct WindowOperator {
    child: BoxedOperator,
    window_functions: Vec<WindowFunctionExpr>,
    output_schema: TableSchema,
    result_rows: Vec<Row>,
    position: usize,
    opened: bool,
}

fn order_by_values_are_peers(
    prev_values: &[Value],
    current_values: &[Value],
    order_by: &[TypedOrderByExpr],
) -> Result<bool> {
    debug_assert_eq!(prev_values.len(), order_by.len());
    debug_assert_eq!(current_values.len(), order_by.len());

    for (order_expr, (prev_value, current_value)) in order_by
        .iter()
        .zip(prev_values.iter().zip(current_values.iter()))
    {
        let asc = order_expr.asc;
        let nulls_first = order_expr.nulls_first;
        if !matches!(
            compare_order_by_values(prev_value, current_value, asc, nulls_first)?,
            std::cmp::Ordering::Equal
        ) {
            return Ok(false);
        }
    }

    Ok(true)
}

impl WindowOperator {
    /// Create a new window operator.
    ///
    /// # Arguments
    /// * `child` - The input operator
    /// * `window_functions` - List of window functions to compute
    pub fn new(child: BoxedOperator, window_functions: Vec<WindowFunctionExpr>) -> Self {
        // Build output schema: input columns + window function results
        let mut columns: Vec<ColumnDef> = child
            .schema()
            .columns
            .iter()
            .map(|c| ColumnDef {
                name: c.name.clone(),
                data_type: c.data_type.clone(),
                nullable: c.nullable,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                generation_expr: None,
                generation_expr_authorized_by: None,
                collation: None,
            })
            .collect();

        for wf in &window_functions {
            columns.push(ColumnDef {
                name: wf.output_name.clone(),
                data_type: wf.output_type.clone(),
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                generation_expr: None,
                generation_expr_authorized_by: None,
                collation: None,
            });
        }

        let output_schema = TableSchema {
            name: "window_result".to_string(),
            table_id: 0,
            columns,
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            rls_enabled: false,
            rls_force: false,
            from_alias: None,
        };

        Self {
            child,
            window_functions,
            output_schema,
            result_rows: Vec::new(),
            position: 0,
            opened: false,
        }
    }

    fn compute_window_functions(
        &self,
        rows: &[Row],
        query_ctx: &QueryContext,
    ) -> Result<Vec<Vec<Value>>> {
        let num_funcs = self.window_functions.len();
        let mut results: Vec<Vec<Value>> = vec![vec![Value::Null; num_funcs]; rows.len()];

        for (wf_idx, wf) in self.window_functions.iter().enumerate() {
            // Partition rows
            let mut partitions: HashMap<Vec<u8>, Vec<usize>> = HashMap::new();
            for (row_idx, row) in rows.iter().enumerate() {
                let mut key = Vec::new();
                for expr in &wf.partition_by {
                    key.push(eval_typed_expr(expr, row, query_ctx)?);
                }
                let key_bytes = encode_values_key(&key);
                partitions.entry(key_bytes).or_default().push(row_idx);
            }

            // Process each partition
            for (_partition_key, mut row_indices) in partitions {
                // Sort within partition if ORDER BY specified
                if !wf.order_by.is_empty() {
                    // Precompute ORDER BY keys so eval errors are caught
                    // before entering the sort closure (which can't return Result).
                    let mut order_key_map: HashMap<usize, Vec<Value>> = HashMap::new();
                    for &row_idx in &row_indices {
                        let mut keys = Vec::with_capacity(wf.order_by.len());
                        for order_expr in &wf.order_by {
                            keys.push(eval_typed_expr(
                                &order_expr.expr,
                                &rows[row_idx],
                                query_ctx,
                            )?);
                        }
                        order_key_map.insert(row_idx, keys);
                    }

                    sort_by_fallible(&mut row_indices, |&a, &b| {
                        let keys_a = &order_key_map[&a];
                        let keys_b = &order_key_map[&b];
                        for (i, order_expr) in wf.order_by.iter().enumerate() {
                            let asc = order_expr.asc;
                            let nulls_first = order_expr.nulls_first;
                            let ord =
                                compare_order_by_values(&keys_a[i], &keys_b[i], asc, nulls_first)?;
                            if !matches!(ord, std::cmp::Ordering::Equal) {
                                return Ok(ord);
                            }
                        }
                        Ok(std::cmp::Ordering::Equal)
                    })?;
                }

                // Compute peer groups for RANGE/GROUPS frame mode support
                let peer_groups = Self::compute_peer_groups(rows, wf, &row_indices, query_ctx)?;

                // Compute function for this partition
                match wf.func_name.as_str() {
                    "row_number" => ranking::compute_row_number(&row_indices, wf_idx, &mut results),
                    "rank" => {
                        ranking::compute_rank(&peer_groups, &row_indices, wf_idx, &mut results)
                    }
                    "dense_rank" => ranking::compute_dense_rank(
                        &peer_groups,
                        &row_indices,
                        wf_idx,
                        &mut results,
                    ),
                    "ntile" => {
                        ranking::compute_ntile(wf, &row_indices, wf_idx, &mut results, query_ctx)?
                    }
                    "percent_rank" => ranking::compute_percent_rank(
                        &peer_groups,
                        &row_indices,
                        wf_idx,
                        &mut results,
                    ),
                    "cume_dist" => {
                        ranking::compute_cume_dist(&peer_groups, &row_indices, wf_idx, &mut results)
                    }
                    "sum" => self.compute_sum(
                        rows,
                        wf,
                        &row_indices,
                        wf_idx,
                        &mut results,
                        &peer_groups,
                        query_ctx,
                    )?,
                    "count" => self.compute_count(
                        rows,
                        wf,
                        &row_indices,
                        wf_idx,
                        &mut results,
                        &peer_groups,
                        query_ctx,
                    )?,
                    "avg" => self.compute_avg(
                        rows,
                        wf,
                        &row_indices,
                        wf_idx,
                        &mut results,
                        &peer_groups,
                        query_ctx,
                    )?,
                    "min" => self.compute_min(
                        rows,
                        wf,
                        &row_indices,
                        wf_idx,
                        &mut results,
                        &peer_groups,
                        query_ctx,
                    )?,
                    "max" => self.compute_max(
                        rows,
                        wf,
                        &row_indices,
                        wf_idx,
                        &mut results,
                        &peer_groups,
                        query_ctx,
                    )?,
                    "lag" => {
                        self.compute_lag(rows, wf, &row_indices, wf_idx, &mut results, query_ctx)?
                    }
                    "lead" => {
                        self.compute_lead(rows, wf, &row_indices, wf_idx, &mut results, query_ctx)?
                    }
                    "first_value" => self.compute_first_value(
                        rows,
                        wf,
                        &row_indices,
                        wf_idx,
                        &mut results,
                        &peer_groups,
                        query_ctx,
                    )?,
                    "last_value" => self.compute_last_value(
                        rows,
                        wf,
                        &row_indices,
                        wf_idx,
                        &mut results,
                        &peer_groups,
                        query_ctx,
                    )?,
                    "nth_value" => self.compute_nth_value(
                        rows,
                        wf,
                        &row_indices,
                        wf_idx,
                        &mut results,
                        &peer_groups,
                        query_ctx,
                    )?,
                    _ => {
                        return Err(SqlError::Unsupported(format!(
                            "Unsupported window function: {}",
                            wf.func_name
                        ))
                        .into())
                    }
                }
            }
        }

        Ok(results)
    }

    /// Compute peer group boundaries for a sorted partition.
    /// Returns a vec of (group_start, group_end) ranges, where each range
    /// contains rows with equal ORDER BY values.
    fn compute_peer_groups(
        rows: &[Row],
        wf: &WindowFunctionExpr,
        row_indices: &[usize],
        query_ctx: &QueryContext,
    ) -> Result<Vec<(usize, usize)>> {
        if wf.order_by.is_empty() {
            // No ORDER BY: all rows are peers
            return Ok(vec![(0, row_indices.len())]);
        }
        let mut groups = Vec::new();
        let mut group_start = 0usize;
        let mut prev_values: Option<Vec<Value>> = None;
        for (pos, &row_idx) in row_indices.iter().enumerate() {
            let current_values: Vec<Value> = wf
                .order_by
                .iter()
                .map(|o| eval_typed_expr(&o.expr, &rows[row_idx], query_ctx))
                .collect::<Result<Vec<Value>>>()?;
            if let Some(prev) = &prev_values {
                if !order_by_values_are_peers(prev, &current_values, &wf.order_by)? {
                    groups.push((group_start, pos));
                    group_start = pos;
                }
            }
            prev_values = Some(current_values);
        }
        groups.push((group_start, row_indices.len()));
        Ok(groups)
    }

    /// Find which peer group a given position belongs to (binary search, O(log G)).
    fn peer_group_of(peer_groups: &[(usize, usize)], pos: usize) -> usize {
        debug_assert!(!peer_groups.is_empty(), "peer_groups must not be empty");
        // Binary search: find the last group whose start <= pos
        let idx = peer_groups.partition_point(|&(start, _)| start <= pos);
        // partition_point returns the first index where start > pos,
        // so the group is at idx - 1
        idx.saturating_sub(1)
    }

    fn get_frame_bounds(
        &self,
        wf: &WindowFunctionExpr,
        current_pos: usize,
        partition_size: usize,
        peer_groups: &[(usize, usize)],
        query_ctx: &QueryContext,
    ) -> Result<(usize, usize)> {
        let frame = match &wf.window_frame {
            Some(f) => f,
            None => {
                // PostgreSQL default: no explicit frame
                // - Without ORDER BY: whole partition (RANGE UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING)
                // - With ORDER BY: RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
                if wf.order_by.is_empty() {
                    return Ok((0, partition_size));
                } else {
                    // Default is RANGE CURRENT ROW -- include all peers of current row
                    let g = Self::peer_group_of(peer_groups, current_pos);
                    return Ok((0, peer_groups[g].1));
                }
            }
        };

        let units = &frame.units;

        let resolve_start = |bound: &WindowFrameBound| -> Result<usize> {
            match bound {
                WindowFrameBound::Preceding(None) => Ok(0), // UNBOUNDED PRECEDING
                WindowFrameBound::Preceding(Some(n)) => {
                    let offset = self.eval_frame_bound_offset(n, query_ctx)?;
                    match units {
                        WindowFrameUnits::Rows => Ok(current_pos.saturating_sub(offset)),
                        WindowFrameUnits::Groups => {
                            let g = Self::peer_group_of(peer_groups, current_pos);
                            let target_g = g.saturating_sub(offset);
                            Ok(peer_groups[target_g].0)
                        }
                        WindowFrameUnits::Range => {
                            // For RANGE N PRECEDING, we need value-based comparison.
                            // Currently only support UNBOUNDED and CURRENT ROW for RANGE with offsets.
                            // For numeric ORDER BY, find first row where value >= current - N.
                            // This requires access to row data; fall back to peer-group-based for now.
                            let g = Self::peer_group_of(peer_groups, current_pos);
                            let target_g = g.saturating_sub(offset);
                            Ok(peer_groups[target_g].0)
                        }
                    }
                }
                WindowFrameBound::CurrentRow => match units {
                    WindowFrameUnits::Rows => Ok(current_pos),
                    WindowFrameUnits::Groups | WindowFrameUnits::Range => {
                        let g = Self::peer_group_of(peer_groups, current_pos);
                        Ok(peer_groups[g].0)
                    }
                },
                WindowFrameBound::Following(Some(n)) => {
                    let offset = self.eval_frame_bound_offset(n, query_ctx)?;
                    match units {
                        WindowFrameUnits::Rows => {
                            Ok(current_pos.saturating_add(offset).min(partition_size))
                        }
                        WindowFrameUnits::Groups => {
                            let g = Self::peer_group_of(peer_groups, current_pos);
                            let target_g = (g + offset).min(peer_groups.len() - 1);
                            Ok(peer_groups[target_g].0)
                        }
                        WindowFrameUnits::Range => {
                            let g = Self::peer_group_of(peer_groups, current_pos);
                            let target_g = (g + offset).min(peer_groups.len() - 1);
                            Ok(peer_groups[target_g].0)
                        }
                    }
                }
                WindowFrameBound::Following(None) => Ok(partition_size), // UNBOUNDED FOLLOWING
            }
        };

        let resolve_end = |bound: &WindowFrameBound| -> Result<usize> {
            match bound {
                WindowFrameBound::Preceding(None) => Ok(0),
                WindowFrameBound::Preceding(Some(n)) => {
                    let offset = self.eval_frame_bound_offset(n, query_ctx)?;
                    match units {
                        WindowFrameUnits::Rows => {
                            Ok(current_pos.saturating_add(1).saturating_sub(offset))
                        }
                        WindowFrameUnits::Groups => {
                            let g = Self::peer_group_of(peer_groups, current_pos);
                            let target_g = g.saturating_sub(offset);
                            Ok(peer_groups[target_g].1)
                        }
                        WindowFrameUnits::Range => {
                            let g = Self::peer_group_of(peer_groups, current_pos);
                            let target_g = g.saturating_sub(offset);
                            Ok(peer_groups[target_g].1)
                        }
                    }
                }
                WindowFrameBound::CurrentRow => match units {
                    WindowFrameUnits::Rows => Ok(current_pos + 1),
                    WindowFrameUnits::Groups | WindowFrameUnits::Range => {
                        let g = Self::peer_group_of(peer_groups, current_pos);
                        Ok(peer_groups[g].1)
                    }
                },
                WindowFrameBound::Following(Some(n)) => {
                    let offset = self.eval_frame_bound_offset(n, query_ctx)?;
                    match units {
                        WindowFrameUnits::Rows => Ok(current_pos
                            .saturating_add(1)
                            .saturating_add(offset)
                            .min(partition_size)),
                        WindowFrameUnits::Groups => {
                            let g = Self::peer_group_of(peer_groups, current_pos);
                            let target_g = (g + offset).min(peer_groups.len() - 1);
                            Ok(peer_groups[target_g].1)
                        }
                        WindowFrameUnits::Range => {
                            let g = Self::peer_group_of(peer_groups, current_pos);
                            let target_g = (g + offset).min(peer_groups.len() - 1);
                            Ok(peer_groups[target_g].1)
                        }
                    }
                }
                WindowFrameBound::Following(None) => Ok(partition_size), // UNBOUNDED FOLLOWING
            }
        };

        let start = resolve_start(&frame.start)?;
        let end = match &frame.end {
            Some(bound) => resolve_end(bound)?,
            None => {
                // Shorthand form: default end is CURRENT ROW
                match units {
                    WindowFrameUnits::Rows => current_pos + 1,
                    WindowFrameUnits::Groups | WindowFrameUnits::Range => {
                        let g = Self::peer_group_of(peer_groups, current_pos);
                        peer_groups[g].1
                    }
                }
            }
        };

        Ok((start, end))
    }

    fn eval_frame_bound_offset(&self, expr: &TypedExpr, query_ctx: &QueryContext) -> Result<usize> {
        let val = eval_typed_expr(expr, &Row::new(vec![]), query_ctx)?;
        match val {
            Value::Int32(n) if n >= 0 => Ok(n as usize),
            Value::Int64(n) if n >= 0 => Ok(n as usize),
            other => Err(anyhow!(
                "Invalid window frame bound: expected non-negative integer, got {:?}",
                other
            )),
        }
    }

    /// Check if a row passes the FILTER (WHERE ...) clause for a window aggregate.
    fn passes_filter(
        filter_expr: &Option<TypedExpr>,
        row: &Row,
        query_ctx: &QueryContext,
    ) -> Result<bool> {
        match filter_expr {
            None => Ok(true),
            Some(expr) => {
                let val = eval_typed_expr(expr, row, query_ctx)?;
                Ok(matches!(val, Value::Boolean(true)))
            }
        }
    }

    fn value_to_decimal(&self, val: &Value) -> Option<rust_decimal::Decimal> {
        match val {
            Value::Int32(n) => Some(rust_decimal::Decimal::from(*n)),
            Value::Int64(n) => Some(rust_decimal::Decimal::from(*n)),
            Value::Float64(f) => rust_decimal::Decimal::try_from(*f).ok(),
            Value::Numeric(d) => Some(*d),
            _ => None,
        }
    }
}

#[async_trait]
impl PhysicalOperator for WindowOperator {
    fn schema(&self) -> &TableSchema {
        &self.output_schema
    }

    async fn open(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.child.open(ctx).await?;

        let input_rows = collect_all(self.child.as_mut(), ctx).await?;

        // Compute window function values
        let window_results = self.compute_window_functions(&input_rows, ctx.query_ctx)?;

        // Build result rows: input columns + window function results
        self.result_rows.clear();
        for (row_idx, row) in input_rows.into_iter().enumerate() {
            let mut values = row.values;
            values.extend(window_results[row_idx].clone());
            self.result_rows.push(Row::new(values));
        }

        self.position = 0;
        self.opened = true;
        Ok(())
    }

    async fn next(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>> {
        if !self.opened {
            return Err(anyhow!("Operator not opened"));
        }

        if self.position < self.result_rows.len() {
            let row = self.result_rows[self.position].clone();
            self.position += 1;
            Ok(Some(row))
        } else {
            Ok(None)
        }
    }

    async fn close(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.child.close(ctx).await?;
        self.result_rows.clear();
        self.opened = false;
        Ok(())
    }

    fn children(&self) -> Vec<&dyn PhysicalOperator> {
        vec![self.child.as_ref()]
    }

    fn children_mut(&mut self) -> Vec<&mut dyn PhysicalOperator> {
        vec![self.child.as_mut()]
    }

    fn name(&self) -> &'static str {
        "Window"
    }

    fn explain_info(&self) -> Option<String> {
        let funcs: Vec<String> = self
            .window_functions
            .iter()
            .map(|wf| wf.func_name.clone())
            .collect();
        Some(format!("functions=[{}]", funcs.join(", ")))
    }
}
