//! Window function operator for Volcano-style execution
//!
//! This operator computes window functions over partitioned and ordered data.

use std::collections::HashMap;

use crate::sql::analyzer::types::{
    TypedExpr, TypedOrderByExpr, WindowFrame, WindowFrameBound, WindowFrameUnits,
};
use crate::sql::error::SqlError;
use anyhow::{anyhow, Result};
use async_trait::async_trait;

use super::{collect_all, BoxedOperator, ExecutionContext, PhysicalOperator};
use crate::sql::expr::compare_order_by_values;
use crate::sql::expr::operators::sort_by_fallible;
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::pg_numeric::pg_numeric_div;
use crate::sql::query_context::QueryContext;
use crate::sql::value_key::serialize_values_for_key;
use crate::types::{ColumnDef, DataType, Row, TableSchema, Value};

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
                let key_bytes = serialize_values_for_key(&key)?;
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
                    "row_number" => self.compute_row_number(&row_indices, wf_idx, &mut results),
                    "rank" => self.compute_rank(&peer_groups, &row_indices, wf_idx, &mut results),
                    "dense_rank" => {
                        self.compute_dense_rank(&peer_groups, &row_indices, wf_idx, &mut results)
                    }
                    "ntile" => {
                        self.compute_ntile(wf, &row_indices, wf_idx, &mut results, query_ctx)?
                    }
                    "percent_rank" => {
                        self.compute_percent_rank(&peer_groups, &row_indices, wf_idx, &mut results)
                    }
                    "cume_dist" => {
                        self.compute_cume_dist(&peer_groups, &row_indices, wf_idx, &mut results)
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

    fn compute_row_number(&self, row_indices: &[usize], wf_idx: usize, results: &mut [Vec<Value>]) {
        for (pos, &row_idx) in row_indices.iter().enumerate() {
            results[row_idx][wf_idx] = Value::Int64((pos + 1) as i64);
        }
    }

    fn compute_rank(
        &self,
        peer_groups: &[(usize, usize)],
        row_indices: &[usize],
        wf_idx: usize,
        results: &mut [Vec<Value>],
    ) {
        for (g_idx, &(g_start, g_end)) in peer_groups.iter().enumerate() {
            let rank = (g_start + 1) as i64; // rank = position of first peer + 1
            for pos in g_start..g_end {
                let _ = g_idx; // suppress unused warning
                results[row_indices[pos]][wf_idx] = Value::Int64(rank);
            }
        }
    }

    fn compute_dense_rank(
        &self,
        peer_groups: &[(usize, usize)],
        row_indices: &[usize],
        wf_idx: usize,
        results: &mut [Vec<Value>],
    ) {
        for (g_idx, &(g_start, g_end)) in peer_groups.iter().enumerate() {
            let rank = (g_idx + 1) as i64;
            for pos in g_start..g_end {
                results[row_indices[pos]][wf_idx] = Value::Int64(rank);
            }
        }
    }

    fn compute_ntile(
        &self,
        wf: &WindowFunctionExpr,
        row_indices: &[usize],
        wf_idx: usize,
        results: &mut [Vec<Value>],
        query_ctx: &QueryContext,
    ) -> Result<()> {
        let n = match &wf.arg_expr {
            Some(expr) => {
                let val = eval_typed_expr(expr, &Row::new(vec![]), query_ctx)?;
                match val {
                    Value::Int32(v) if v > 0 => v as usize,
                    Value::Int64(v) if v > 0 => v as usize,
                    other => {
                        return Err(anyhow!(
                            "NTILE argument must be a positive integer, got {:?}",
                            other
                        ))
                    }
                }
            }
            None => return Err(anyhow!("NTILE requires exactly one argument")),
        };

        let total = row_indices.len();
        let base_size = total / n;
        let remainder = total % n;

        for (pos, &row_idx) in row_indices.iter().enumerate() {
            // First `remainder` buckets get base_size+1 rows, rest get base_size
            let bucket = if base_size == 0 {
                // More buckets than rows: each row gets its own bucket up to n
                pos + 1
            } else if pos < remainder * (base_size + 1) {
                pos / (base_size + 1) + 1
            } else {
                let adjusted = pos - remainder * (base_size + 1);
                remainder + adjusted / base_size + 1
            };
            results[row_idx][wf_idx] = Value::Int64(bucket as i64);
        }
        Ok(())
    }

    fn compute_percent_rank(
        &self,
        peer_groups: &[(usize, usize)],
        row_indices: &[usize],
        wf_idx: usize,
        results: &mut [Vec<Value>],
    ) {
        let total = row_indices.len();
        for &(g_start, g_end) in peer_groups {
            let value = if total <= 1 {
                0.0f64
            } else {
                g_start as f64 / (total - 1) as f64
            };
            for pos in g_start..g_end {
                results[row_indices[pos]][wf_idx] = Value::Float64(value);
            }
        }
    }

    fn compute_cume_dist(
        &self,
        peer_groups: &[(usize, usize)],
        row_indices: &[usize],
        wf_idx: usize,
        results: &mut [Vec<Value>],
    ) {
        let total = row_indices.len();
        for &(_g_start, g_end) in peer_groups {
            let value = g_end as f64 / total as f64;
            for pos in _g_start..g_end {
                results[row_indices[pos]][wf_idx] = Value::Float64(value);
            }
        }
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
                    // Default is RANGE CURRENT ROW — include all peers of current row
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

    fn compute_sum(
        &self,
        rows: &[Row],
        wf: &WindowFunctionExpr,
        row_indices: &[usize],
        wf_idx: usize,
        results: &mut [Vec<Value>],
        peer_groups: &[(usize, usize)],
        query_ctx: &QueryContext,
    ) -> Result<()> {
        let partition_size = row_indices.len();
        for (pos, &row_idx) in row_indices.iter().enumerate() {
            let (start, end) =
                self.get_frame_bounds(wf, pos, partition_size, peer_groups, query_ctx)?;
            let mut sum = rust_decimal::Decimal::ZERO;
            let mut has_value = false;
            for i in start..end {
                if !Self::passes_filter(&wf.filter_expr, &rows[row_indices[i]], query_ctx)? {
                    continue;
                }
                if let Some(expr) = &wf.arg_expr {
                    let val = eval_typed_expr(expr, &rows[row_indices[i]], query_ctx)?;
                    if let Some(n) = self.value_to_decimal(&val) {
                        sum += n;
                        has_value = true;
                    }
                }
            }
            results[row_idx][wf_idx] = if has_value {
                Value::Numeric(sum)
            } else {
                Value::Null
            };
        }
        Ok(())
    }

    fn compute_count(
        &self,
        rows: &[Row],
        wf: &WindowFunctionExpr,
        row_indices: &[usize],
        wf_idx: usize,
        results: &mut [Vec<Value>],
        peer_groups: &[(usize, usize)],
        query_ctx: &QueryContext,
    ) -> Result<()> {
        let partition_size = row_indices.len();
        for (pos, &row_idx) in row_indices.iter().enumerate() {
            let (start, end) =
                self.get_frame_bounds(wf, pos, partition_size, peer_groups, query_ctx)?;
            let mut count = 0i64;
            for i in start..end.min(partition_size) {
                if !Self::passes_filter(&wf.filter_expr, &rows[row_indices[i]], query_ctx)? {
                    continue;
                }
                if let Some(expr) = &wf.arg_expr {
                    let val = eval_typed_expr(expr, &rows[row_indices[i]], query_ctx)?;
                    if !matches!(val, Value::Null) {
                        count += 1;
                    }
                } else {
                    // COUNT(*) — count all rows
                    count += 1;
                }
            }
            results[row_idx][wf_idx] = Value::Int64(count);
        }
        Ok(())
    }

    fn compute_avg(
        &self,
        rows: &[Row],
        wf: &WindowFunctionExpr,
        row_indices: &[usize],
        wf_idx: usize,
        results: &mut [Vec<Value>],
        peer_groups: &[(usize, usize)],
        query_ctx: &QueryContext,
    ) -> Result<()> {
        let partition_size = row_indices.len();
        for (pos, &row_idx) in row_indices.iter().enumerate() {
            let (start, end) =
                self.get_frame_bounds(wf, pos, partition_size, peer_groups, query_ctx)?;
            let mut sum = rust_decimal::Decimal::ZERO;
            let mut count = 0i64;
            for i in start..end {
                if !Self::passes_filter(&wf.filter_expr, &rows[row_indices[i]], query_ctx)? {
                    continue;
                }
                if let Some(expr) = &wf.arg_expr {
                    let val = eval_typed_expr(expr, &rows[row_indices[i]], query_ctx)?;
                    if let Some(n) = self.value_to_decimal(&val) {
                        sum += n;
                        count += 1;
                    }
                }
            }
            results[row_idx][wf_idx] = if count > 0 {
                Value::Numeric(pg_numeric_div(sum, rust_decimal::Decimal::from(count)))
            } else {
                Value::Null
            };
        }
        Ok(())
    }

    fn compute_min(
        &self,
        rows: &[Row],
        wf: &WindowFunctionExpr,
        row_indices: &[usize],
        wf_idx: usize,
        results: &mut [Vec<Value>],
        peer_groups: &[(usize, usize)],
        query_ctx: &QueryContext,
    ) -> Result<()> {
        let partition_size = row_indices.len();
        for (pos, &row_idx) in row_indices.iter().enumerate() {
            let (start, end) =
                self.get_frame_bounds(wf, pos, partition_size, peer_groups, query_ctx)?;
            let mut min_val: Option<Value> = None;
            for i in start..end {
                if !Self::passes_filter(&wf.filter_expr, &rows[row_indices[i]], query_ctx)? {
                    continue;
                }
                if let Some(expr) = &wf.arg_expr {
                    let val = eval_typed_expr(expr, &rows[row_indices[i]], query_ctx)?;
                    if !matches!(val, Value::Null) {
                        min_val = Some(match min_val {
                            None => val,
                            Some(m) => {
                                if crate::sql::expr::compare_values(&val, &m)
                                    .map(|c| c < 0)
                                    .unwrap_or(false)
                                {
                                    val
                                } else {
                                    m
                                }
                            }
                        });
                    }
                }
            }
            results[row_idx][wf_idx] = min_val.unwrap_or(Value::Null);
        }
        Ok(())
    }

    fn compute_max(
        &self,
        rows: &[Row],
        wf: &WindowFunctionExpr,
        row_indices: &[usize],
        wf_idx: usize,
        results: &mut [Vec<Value>],
        peer_groups: &[(usize, usize)],
        query_ctx: &QueryContext,
    ) -> Result<()> {
        let partition_size = row_indices.len();
        for (pos, &row_idx) in row_indices.iter().enumerate() {
            let (start, end) =
                self.get_frame_bounds(wf, pos, partition_size, peer_groups, query_ctx)?;
            let mut max_val: Option<Value> = None;
            for i in start..end {
                if !Self::passes_filter(&wf.filter_expr, &rows[row_indices[i]], query_ctx)? {
                    continue;
                }
                if let Some(expr) = &wf.arg_expr {
                    let val = eval_typed_expr(expr, &rows[row_indices[i]], query_ctx)?;
                    if !matches!(val, Value::Null) {
                        max_val = Some(match max_val {
                            None => val,
                            Some(m) => {
                                if crate::sql::expr::compare_values(&val, &m)
                                    .map(|c| c > 0)
                                    .unwrap_or(false)
                                {
                                    val
                                } else {
                                    m
                                }
                            }
                        });
                    }
                }
            }
            results[row_idx][wf_idx] = max_val.unwrap_or(Value::Null);
        }
        Ok(())
    }

    fn compute_lag(
        &self,
        rows: &[Row],
        wf: &WindowFunctionExpr,
        row_indices: &[usize],
        wf_idx: usize,
        results: &mut [Vec<Value>],
        query_ctx: &QueryContext,
    ) -> Result<()> {
        let empty_row = Row::new(vec![]);
        let offset = match wf.offset_expr.as_ref() {
            None => 1,
            Some(expr) => match eval_typed_expr(expr, &empty_row, query_ctx) {
                Ok(Value::Int32(n)) => {
                    if n < 0 {
                        return Err(anyhow!("LAG offset must be non-negative"));
                    }
                    n as usize
                }
                Ok(Value::Int64(n)) => {
                    if n < 0 {
                        return Err(anyhow!("LAG offset must be non-negative"));
                    }
                    usize::try_from(n).map_err(|_| anyhow!("LAG offset too large: {}", n))?
                }
                _ => 1,
            },
        };

        let default_value = match &wf.default_value_expr {
            Some(e) => eval_typed_expr(e, &empty_row, query_ctx)?,
            None => Value::Null,
        };

        for (pos, &row_idx) in row_indices.iter().enumerate() {
            results[row_idx][wf_idx] = if pos >= offset {
                if let Some(expr) = &wf.arg_expr {
                    eval_typed_expr(expr, &rows[row_indices[pos - offset]], query_ctx)?
                } else {
                    Value::Null
                }
            } else {
                default_value.clone()
            };
        }
        Ok(())
    }

    fn compute_lead(
        &self,
        rows: &[Row],
        wf: &WindowFunctionExpr,
        row_indices: &[usize],
        wf_idx: usize,
        results: &mut [Vec<Value>],
        query_ctx: &QueryContext,
    ) -> Result<()> {
        let empty_row = Row::new(vec![]);
        let offset = match wf.offset_expr.as_ref() {
            None => 1,
            Some(expr) => match eval_typed_expr(expr, &empty_row, query_ctx) {
                Ok(Value::Int32(n)) => {
                    if n < 0 {
                        return Err(anyhow!("LEAD offset must be non-negative"));
                    }
                    n as usize
                }
                Ok(Value::Int64(n)) => {
                    if n < 0 {
                        return Err(anyhow!("LEAD offset must be non-negative"));
                    }
                    usize::try_from(n).map_err(|_| anyhow!("LEAD offset too large: {}", n))?
                }
                _ => 1,
            },
        };

        let default_value = match &wf.default_value_expr {
            Some(e) => eval_typed_expr(e, &empty_row, query_ctx)?,
            None => Value::Null,
        };

        let partition_size = row_indices.len();
        for (pos, &row_idx) in row_indices.iter().enumerate() {
            results[row_idx][wf_idx] = if pos + offset < partition_size {
                if let Some(expr) = &wf.arg_expr {
                    eval_typed_expr(expr, &rows[row_indices[pos + offset]], query_ctx)?
                } else {
                    Value::Null
                }
            } else {
                default_value.clone()
            };
        }
        Ok(())
    }

    fn compute_first_value(
        &self,
        rows: &[Row],
        wf: &WindowFunctionExpr,
        row_indices: &[usize],
        wf_idx: usize,
        results: &mut [Vec<Value>],
        peer_groups: &[(usize, usize)],
        query_ctx: &QueryContext,
    ) -> Result<()> {
        let partition_size = row_indices.len();
        for (pos, &row_idx) in row_indices.iter().enumerate() {
            let (start, end) =
                self.get_frame_bounds(wf, pos, partition_size, peer_groups, query_ctx)?;
            results[row_idx][wf_idx] = if start < end && start < partition_size {
                if let Some(expr) = &wf.arg_expr {
                    eval_typed_expr(expr, &rows[row_indices[start]], query_ctx)?
                } else {
                    Value::Null
                }
            } else {
                Value::Null
            };
        }
        Ok(())
    }

    fn compute_last_value(
        &self,
        rows: &[Row],
        wf: &WindowFunctionExpr,
        row_indices: &[usize],
        wf_idx: usize,
        results: &mut [Vec<Value>],
        peer_groups: &[(usize, usize)],
        query_ctx: &QueryContext,
    ) -> Result<()> {
        let partition_size = row_indices.len();
        for (pos, &row_idx) in row_indices.iter().enumerate() {
            let (start, end) =
                self.get_frame_bounds(wf, pos, partition_size, peer_groups, query_ctx)?;
            results[row_idx][wf_idx] = if start < end && end > 0 && end <= partition_size {
                if let Some(expr) = &wf.arg_expr {
                    eval_typed_expr(expr, &rows[row_indices[end - 1]], query_ctx)?
                } else {
                    Value::Null
                }
            } else {
                Value::Null
            };
        }
        Ok(())
    }

    fn compute_nth_value(
        &self,
        rows: &[Row],
        wf: &WindowFunctionExpr,
        row_indices: &[usize],
        wf_idx: usize,
        results: &mut [Vec<Value>],
        peer_groups: &[(usize, usize)],
        query_ctx: &QueryContext,
    ) -> Result<()> {
        // NTH_VALUE(expr, n): the second argument is the 1-based position
        let n = match &wf.offset_expr {
            Some(expr) => {
                let val = eval_typed_expr(expr, &Row::new(vec![]), query_ctx)?;
                match val {
                    Value::Int32(v) if v > 0 => v as usize,
                    Value::Int64(v) if v > 0 => v as usize,
                    other => {
                        return Err(anyhow!(
                            "NTH_VALUE second argument must be a positive integer, got {:?}",
                            other
                        ))
                    }
                }
            }
            None => return Err(anyhow!("NTH_VALUE requires two arguments")),
        };

        let partition_size = row_indices.len();
        for (pos, &row_idx) in row_indices.iter().enumerate() {
            let (start, end) =
                self.get_frame_bounds(wf, pos, partition_size, peer_groups, query_ctx)?;
            let target_pos = start + n - 1; // convert 1-based to 0-based
            results[row_idx][wf_idx] = if target_pos < end && target_pos < partition_size {
                if let Some(expr) = &wf.arg_expr {
                    eval_typed_expr(expr, &rows[row_indices[target_pos]], query_ctx)?
                } else {
                    Value::Null
                }
            } else {
                Value::Null
            };
        }
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::analyzer::types::TypedExprKind;
    use crate::sql::operators::scan::TableScanOperator;

    fn test_schema() -> TableSchema {
        TableSchema {
            name: "sales".to_string(),
            table_id: 1,
            columns: vec![
                ColumnDef {
                    name: "id".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: true,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
                ColumnDef {
                    name: "amount".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![0],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        }
    }

    /// Helper: creates a TypedExpr::ColumnRef for the given column index and name.
    fn col_ref(index: usize, name: &str, data_type: DataType) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: index,
                column_name: name.to_string(),
            },
            data_type,
        }
    }

    #[test]
    fn test_window_operator_creation() {
        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let window_funcs = vec![WindowFunctionExpr {
            func_name: "row_number".to_string(),
            arg_expr: None,
            partition_by: vec![],
            order_by: vec![],
            offset_expr: None,
            default_value_expr: None,
            window_frame: None,
            filter_expr: None,
            output_name: "row_num".to_string(),
            output_type: DataType::Int64,
        }];

        let op = WindowOperator::new(child, window_funcs);

        assert_eq!(op.name(), "Window");
        assert_eq!(op.schema().columns.len(), 3); // 2 input + 1 window
        assert_eq!(op.schema().columns[2].name, "row_num");
    }

    #[test]
    fn test_window_operator_explain() {
        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let window_funcs = vec![
            WindowFunctionExpr {
                func_name: "row_number".to_string(),
                arg_expr: None,
                partition_by: vec![],
                order_by: vec![],
                offset_expr: None,
                default_value_expr: None,
                window_frame: None,
                filter_expr: None,
                output_name: "row_num".to_string(),
                output_type: DataType::Int64,
            },
            WindowFunctionExpr {
                func_name: "sum".to_string(),
                arg_expr: None,
                partition_by: vec![],
                order_by: vec![],
                offset_expr: None,
                default_value_expr: None,
                window_frame: None,
                filter_expr: None,
                output_name: "total".to_string(),
                output_type: DataType::Numeric {
                    precision: None,
                    scale: None,
                },
            },
        ];

        let op = WindowOperator::new(child, window_funcs);

        let info = op.explain_info().unwrap();
        assert!(info.contains("row_number"));
        assert!(info.contains("sum"));
    }

    fn test_schema_with_float_partition() -> TableSchema {
        TableSchema {
            name: "test".to_string(),
            table_id: 1,
            columns: vec![
                ColumnDef {
                    name: "id".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: true,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
                ColumnDef {
                    name: "grp".to_string(),
                    data_type: DataType::Float64,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![0],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        }
    }

    fn test_schema_with_numeric_partition() -> TableSchema {
        TableSchema {
            name: "test".to_string(),
            table_id: 1,
            columns: vec![
                ColumnDef {
                    name: "id".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: true,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
                ColumnDef {
                    name: "grp".to_string(),
                    data_type: DataType::Numeric {
                        precision: None,
                        scale: Some(2),
                    },
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![0],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        }
    }

    #[test]
    fn test_window_operator_partition_by_canonicalizes_float_keys() {
        let schema = test_schema_with_float_partition();
        let child = Box::new(TableScanOperator::new(schema));

        let window_funcs = vec![WindowFunctionExpr {
            func_name: "row_number".to_string(),
            arg_expr: None,
            partition_by: vec![col_ref(1, "grp", DataType::Float64)],
            order_by: vec![],
            offset_expr: None,
            default_value_expr: None,
            window_frame: None,
            filter_expr: None,
            output_name: "row_num".to_string(),
            output_type: DataType::Int64,
        }];

        let op = WindowOperator::new(child, window_funcs);

        let rows = vec![
            Row::new(vec![Value::Int32(1), Value::Float64(-0.0)]),
            Row::new(vec![Value::Int32(2), Value::Float64(0.0)]),
        ];
        let results = op
            .compute_window_functions(&rows, &QueryContext::from_task_locals())
            .unwrap();
        assert_eq!(results[0][0], Value::Int64(1));
        assert_eq!(results[1][0], Value::Int64(2));

        let nan1 = f64::from_bits(0x7ff8_0000_0000_0001);
        let nan2 = f64::from_bits(0x7ff8_0000_0000_0002);
        assert!(nan1.is_nan() && nan2.is_nan());
        let rows = vec![
            Row::new(vec![Value::Int32(1), Value::Float64(nan1)]),
            Row::new(vec![Value::Int32(2), Value::Float64(nan2)]),
        ];
        let results = op
            .compute_window_functions(&rows, &QueryContext::from_task_locals())
            .unwrap();
        assert_eq!(results[0][0], Value::Int64(1));
        assert_eq!(results[1][0], Value::Int64(2));
    }

    #[test]
    fn test_window_operator_partition_by_canonicalizes_numeric_keys() {
        use rust_decimal::Decimal;
        use std::str::FromStr;

        let schema = test_schema_with_numeric_partition();
        let child = Box::new(TableScanOperator::new(schema));

        let window_funcs = vec![WindowFunctionExpr {
            func_name: "row_number".to_string(),
            arg_expr: None,
            partition_by: vec![col_ref(
                1,
                "grp",
                DataType::Numeric {
                    precision: None,
                    scale: Some(2),
                },
            )],
            order_by: vec![],
            offset_expr: None,
            default_value_expr: None,
            window_frame: None,
            filter_expr: None,
            output_name: "row_num".to_string(),
            output_type: DataType::Int64,
        }];

        let op = WindowOperator::new(child, window_funcs);

        let rows = vec![
            Row::new(vec![
                Value::Int32(1),
                Value::Numeric(Decimal::from_str("1.0").unwrap()),
            ]),
            Row::new(vec![
                Value::Int32(2),
                Value::Numeric(Decimal::from_str("1.00").unwrap()),
            ]),
        ];
        let results = op
            .compute_window_functions(&rows, &QueryContext::from_task_locals())
            .unwrap();
        assert_eq!(results[0][0], Value::Int64(1));
        assert_eq!(results[1][0], Value::Int64(2));
    }

    #[test]
    fn test_window_operator_rank_dense_rank_treat_nan_order_keys_as_peers() {
        let schema = test_schema_with_float_partition();
        let child = Box::new(TableScanOperator::new(schema));

        let grp_col = col_ref(1, "grp", DataType::Float64);

        let window_funcs = vec![
            WindowFunctionExpr {
                func_name: "rank".to_string(),
                arg_expr: None,
                partition_by: vec![],
                order_by: vec![TypedOrderByExpr {
                    expr: grp_col.clone(),
                    asc: true,
                    nulls_first: false,
                }],
                offset_expr: None,
                default_value_expr: None,
                window_frame: None,
                filter_expr: None,
                output_name: "rank_val".to_string(),
                output_type: DataType::Int64,
            },
            WindowFunctionExpr {
                func_name: "dense_rank".to_string(),
                arg_expr: None,
                partition_by: vec![],
                order_by: vec![TypedOrderByExpr {
                    expr: grp_col.clone(),
                    asc: true,
                    nulls_first: false,
                }],
                offset_expr: None,
                default_value_expr: None,
                window_frame: None,
                filter_expr: None,
                output_name: "dense_rank_val".to_string(),
                output_type: DataType::Int64,
            },
        ];

        let op = WindowOperator::new(child, window_funcs);

        let nan1 = f64::from_bits(0x7ff8_0000_0000_0001);
        let nan2 = f64::from_bits(0x7ff8_0000_0000_0002);
        assert!(nan1.is_nan() && nan2.is_nan());

        let rows = vec![
            Row::new(vec![Value::Int32(1), Value::Float64(nan1)]),
            Row::new(vec![Value::Int32(2), Value::Float64(nan2)]),
            Row::new(vec![Value::Int32(3), Value::Float64(1.0)]),
            Row::new(vec![Value::Int32(4), Value::Float64(2.0)]),
        ];

        let results = op
            .compute_window_functions(&rows, &QueryContext::from_task_locals())
            .unwrap();

        // ORDER BY grp ASC sorts NaNs last; both NaNs are peers.
        assert_eq!(results[0][0], Value::Int64(3));
        assert_eq!(results[1][0], Value::Int64(3));
        assert_eq!(results[2][0], Value::Int64(1));
        assert_eq!(results[3][0], Value::Int64(2));

        assert_eq!(results[0][1], Value::Int64(3));
        assert_eq!(results[1][1], Value::Int64(3));
        assert_eq!(results[2][1], Value::Int64(1));
        assert_eq!(results[3][1], Value::Int64(2));
    }

    #[test]
    fn test_window_row_number_partitioned() {
        let schema = TableSchema {
            name: "test".to_string(),
            table_id: 1,
            columns: vec![
                ColumnDef {
                    name: "dept".to_string(),
                    data_type: DataType::Text,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
                ColumnDef {
                    name: "id".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: true,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![1],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };

        let child = Box::new(TableScanOperator::new(schema));

        let window_funcs = vec![WindowFunctionExpr {
            func_name: "row_number".to_string(),
            arg_expr: None,
            partition_by: vec![col_ref(0, "dept", DataType::Text)],
            order_by: vec![TypedOrderByExpr {
                expr: col_ref(1, "id", DataType::Int32),
                asc: true,
                nulls_first: false,
            }],
            offset_expr: None,
            default_value_expr: None,
            window_frame: None,
            filter_expr: None,
            output_name: "rn".to_string(),
            output_type: DataType::Int64,
        }];

        let op = WindowOperator::new(child, window_funcs);

        let rows = vec![
            Row::new(vec![Value::Text("A".to_string()), Value::Int32(1)]),
            Row::new(vec![Value::Text("A".to_string()), Value::Int32(2)]),
            Row::new(vec![Value::Text("B".to_string()), Value::Int32(3)]),
            Row::new(vec![Value::Text("B".to_string()), Value::Int32(4)]),
            Row::new(vec![Value::Text("B".to_string()), Value::Int32(5)]),
        ];

        let results = op
            .compute_window_functions(&rows, &QueryContext::from_task_locals())
            .unwrap();

        // Partition A: row_number 1,2; Partition B: row_number 1,2,3
        assert_eq!(results[0][0], Value::Int64(1));
        assert_eq!(results[1][0], Value::Int64(2));
        assert_eq!(results[2][0], Value::Int64(1));
        assert_eq!(results[3][0], Value::Int64(2));
        assert_eq!(results[4][0], Value::Int64(3));
    }

    #[test]
    fn test_window_sum_aggregate() {
        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let window_funcs = vec![WindowFunctionExpr {
            func_name: "sum".to_string(),
            arg_expr: Some(col_ref(1, "amount", DataType::Int32)),
            partition_by: vec![],
            order_by: vec![TypedOrderByExpr {
                expr: col_ref(0, "id", DataType::Int32),
                asc: true,
                nulls_first: false,
            }],
            offset_expr: None,
            default_value_expr: None,
            window_frame: None,
            filter_expr: None,
            output_name: "running_sum".to_string(),
            output_type: DataType::Numeric {
                precision: None,
                scale: None,
            },
        }];

        let op = WindowOperator::new(child, window_funcs);

        let rows = vec![
            Row::new(vec![Value::Int32(1), Value::Int32(10)]),
            Row::new(vec![Value::Int32(2), Value::Int32(20)]),
            Row::new(vec![Value::Int32(3), Value::Int32(30)]),
        ];

        let results = op
            .compute_window_functions(&rows, &QueryContext::from_task_locals())
            .unwrap();

        // Running sum: 10, 30, 60
        assert_eq!(
            results[0][0],
            Value::Numeric(rust_decimal::Decimal::from(10))
        );
        assert_eq!(
            results[1][0],
            Value::Numeric(rust_decimal::Decimal::from(30))
        );
        assert_eq!(
            results[2][0],
            Value::Numeric(rust_decimal::Decimal::from(60))
        );
    }

    #[test]
    fn test_window_count_no_order_by() {
        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let window_funcs = vec![WindowFunctionExpr {
            func_name: "count".to_string(),
            arg_expr: None,
            partition_by: vec![],
            order_by: vec![],
            offset_expr: None,
            default_value_expr: None,
            window_frame: None,
            filter_expr: None,
            output_name: "total_count".to_string(),
            output_type: DataType::Int64,
        }];

        let op = WindowOperator::new(child, window_funcs);

        let rows = vec![
            Row::new(vec![Value::Int32(1), Value::Int32(10)]),
            Row::new(vec![Value::Int32(2), Value::Int32(20)]),
            Row::new(vec![Value::Int32(3), Value::Int32(30)]),
        ];

        let results = op
            .compute_window_functions(&rows, &QueryContext::from_task_locals())
            .unwrap();

        // Without ORDER BY, COUNT(*) OVER() returns total count for all rows
        assert_eq!(results[0][0], Value::Int64(3));
        assert_eq!(results[1][0], Value::Int64(3));
        assert_eq!(results[2][0], Value::Int64(3));
    }
}
