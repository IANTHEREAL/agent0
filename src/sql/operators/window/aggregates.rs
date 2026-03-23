//! Aggregate window functions: sum, count, avg, min, max (window variants).

use anyhow::Result;

use super::{WindowFunctionExpr, WindowOperator};
use crate::model::{Row, Value};
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::pg_numeric::pg_numeric_div;
use crate::sql::query_context::QueryContext;

impl WindowOperator {
    pub(super) fn compute_sum(
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
                        sum = crate::sql::expr::numeric::checked_decimal_add(sum, n)?;
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

    pub(super) fn compute_count(
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
                    // COUNT(*) -- count all rows
                    count += 1;
                }
            }
            results[row_idx][wf_idx] = Value::Int64(count);
        }
        Ok(())
    }

    pub(super) fn compute_avg(
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
                        sum = crate::sql::expr::numeric::checked_decimal_add(sum, n)?;
                        count += 1;
                    }
                }
            }
            results[row_idx][wf_idx] = if count > 0 {
                Value::Numeric(pg_numeric_div(sum, rust_decimal::Decimal::from(count))?)
            } else {
                Value::Null
            };
        }
        Ok(())
    }

    pub(super) fn compute_min(
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

    pub(super) fn compute_max(
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
}
