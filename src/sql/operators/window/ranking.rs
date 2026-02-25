//! Ranking window functions: row_number, rank, dense_rank, ntile, percent_rank, cume_dist.

use anyhow::{anyhow, Result};

use super::WindowFunctionExpr;
use crate::model::{Row, Value};
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::query_context::QueryContext;

/// Compute `row_number()` for a partition.
pub(super) fn compute_row_number(row_indices: &[usize], wf_idx: usize, results: &mut [Vec<Value>]) {
    for (pos, &row_idx) in row_indices.iter().enumerate() {
        results[row_idx][wf_idx] = Value::Int64((pos + 1) as i64);
    }
}

/// Compute `rank()` for a partition using precomputed peer groups.
pub(super) fn compute_rank(
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

/// Compute `dense_rank()` for a partition using precomputed peer groups.
pub(super) fn compute_dense_rank(
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

/// Compute `ntile(n)` for a partition.
pub(super) fn compute_ntile(
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

/// Compute `percent_rank()` for a partition using precomputed peer groups.
pub(super) fn compute_percent_rank(
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

/// Compute `cume_dist()` for a partition using precomputed peer groups.
pub(super) fn compute_cume_dist(
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
