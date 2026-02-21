//! Positional access window functions: lag, lead, first_value, last_value, nth_value.

use anyhow::{anyhow, Result};

use super::{WindowFunctionExpr, WindowOperator};
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::query_context::QueryContext;
use crate::types::{Row, Value};

impl WindowOperator {
    pub(super) fn compute_lag(
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

    pub(super) fn compute_lead(
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

    pub(super) fn compute_first_value(
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

    pub(super) fn compute_last_value(
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

    pub(super) fn compute_nth_value(
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
}
