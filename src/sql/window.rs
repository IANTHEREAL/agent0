use std::collections::HashMap;

use anyhow::{anyhow, Result};
use rust_decimal::prelude::ToPrimitive;
use sqlparser::ast::{
    Expr, FunctionArg, FunctionArgExpr, OrderByExpr, SelectItem, Value as SqlValue, WindowFrame,
    WindowFrameBound, WindowType,
};

use super::expr::{compare_order_by_values, compare_values, eval_expr, eval_expr_join, JoinContext};
use super::value_key::serialize_values_for_key;
use crate::types::{Row, TableSchema, Value};

pub(crate) struct WindowFuncInfo {
    pub proj_idx: usize,
    pub func_name: String,
    pub arg_expr: Option<Expr>,
    pub partition_by: Vec<Expr>,
    pub order_by: Vec<OrderByExpr>,
    pub offset_expr: Option<Expr>,
    pub default_value_expr: Option<Expr>,
    pub window_frame: Option<WindowFrame>,
}

pub(crate) fn extract_window_functions(projection: &[SelectItem]) -> Vec<WindowFuncInfo> {
    let mut result = Vec::new();
    for (idx, item) in projection.iter().enumerate() {
        let func = match item {
            SelectItem::UnnamedExpr(Expr::Function(f)) => Some(f),
            SelectItem::ExprWithAlias {
                expr: Expr::Function(f),
                ..
            } => Some(f),
            _ => None,
        };
        if let Some(f) = func {
            if let Some(WindowType::WindowSpec(spec)) = &f.over {
                let func_name = f
                    .name
                    .0
                    .last()
                    .map(|i| i.value.to_lowercase())
                    .unwrap_or_default();
                let extract_arg = |index: usize| -> Option<Expr> {
                    f.args.get(index).and_then(|a| match a {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e.clone()),
                        _ => None,
                    })
                };
                let arg_expr = extract_arg(0);
                let offset_expr = extract_arg(1);
                let default_value_expr = extract_arg(2);
                result.push(WindowFuncInfo {
                    proj_idx: idx,
                    func_name,
                    arg_expr,
                    partition_by: spec.partition_by.clone(),
                    order_by: spec.order_by.clone(),
                    offset_expr,
                    default_value_expr,
                    window_frame: spec.window_frame.clone(),
                });
            }
        }
    }
    result
}

pub(crate) fn compute_window_functions(
    rows: &[Row],
    schema: &TableSchema,
    window_funcs: &[WindowFuncInfo],
) -> Result<Vec<Vec<Value>>> {
    let mut results: Vec<Vec<Value>> = vec![vec![Value::Null; window_funcs.len()]; rows.len()];

    for (wf_idx, wf) in window_funcs.iter().enumerate() {
        let mut partitions: HashMap<Vec<u8>, Vec<usize>> = HashMap::new();
        for (row_idx, row) in rows.iter().enumerate() {
            let mut key = Vec::new();
            for expr in &wf.partition_by {
                key.push(eval_expr(expr, Some(row), Some(schema))?);
            }
            let key_bytes = serialize_values_for_key(&key).unwrap_or_default();
            partitions.entry(key_bytes).or_default().push(row_idx);
        }

        for (_partition_key, mut row_indices) in partitions {
            if !wf.order_by.is_empty() {
                row_indices.sort_by(|&a, &b| {
                    for order_expr in &wf.order_by {
                        let val_a = eval_expr(&order_expr.expr, Some(&rows[a]), Some(schema))
                            .unwrap_or(Value::Null);
                        let val_b = eval_expr(&order_expr.expr, Some(&rows[b]), Some(schema))
                            .unwrap_or(Value::Null);
                        let asc = order_expr.asc.unwrap_or(true);
                        let nulls_first = order_expr.nulls_first.unwrap_or(!asc);
                        let ord = compare_order_by_values(&val_a, &val_b, asc, nulls_first);
                        if !matches!(ord, std::cmp::Ordering::Equal) {
                            return ord;
                        }
                    }
                    std::cmp::Ordering::Equal
                });
            }

            match wf.func_name.as_str() {
                "row_number" => compute_row_number(&row_indices, wf_idx, &mut results),
                "rank" => compute_rank(rows, schema, wf, &row_indices, wf_idx, &mut results),
                "dense_rank" => {
                    compute_dense_rank(rows, schema, wf, &row_indices, wf_idx, &mut results)
                }
                "sum" => compute_sum(rows, schema, wf, &row_indices, wf_idx, &mut results)?,
                "count" => compute_count(wf, &row_indices, wf_idx, &mut results),
                "avg" => compute_avg(rows, schema, wf, &row_indices, wf_idx, &mut results)?,
                "min" => compute_min(rows, schema, wf, &row_indices, wf_idx, &mut results)?,
                "max" => compute_max(rows, schema, wf, &row_indices, wf_idx, &mut results)?,
                "lag" => compute_lag(rows, schema, wf, &row_indices, wf_idx, &mut results)?,
                "lead" => compute_lead(rows, schema, wf, &row_indices, wf_idx, &mut results)?,
                "first_value" => {
                    compute_first_value(rows, schema, wf, &row_indices, wf_idx, &mut results)?
                }
                "last_value" => {
                    compute_last_value(rows, schema, wf, &row_indices, wf_idx, &mut results)?
                }
                _ => return Err(anyhow!("Unsupported window function: {}", wf.func_name)),
            }
        }
    }

    Ok(results)
}

fn compute_row_number(row_indices: &[usize], wf_idx: usize, results: &mut [Vec<Value>]) {
    for (pos, &row_idx) in row_indices.iter().enumerate() {
        results[row_idx][wf_idx] = Value::Int64((pos + 1) as i64);
    }
}

fn compute_rank(
    rows: &[Row],
    schema: &TableSchema,
    wf: &WindowFuncInfo,
    row_indices: &[usize],
    wf_idx: usize,
    results: &mut [Vec<Value>],
) {
    let mut current_rank = 1i64;
    let mut prev_values: Option<Vec<Value>> = None;
    for (pos, &row_idx) in row_indices.iter().enumerate() {
        let current_values: Vec<Value> = wf
            .order_by
            .iter()
            .map(|o| eval_expr(&o.expr, Some(&rows[row_idx]), Some(schema)).unwrap_or(Value::Null))
            .collect();
        if let Some(prev) = &prev_values {
            if !order_by_values_are_peers(prev, &current_values, &wf.order_by) {
                current_rank = (pos + 1) as i64;
            }
        }
        results[row_idx][wf_idx] = Value::Int64(current_rank);
        prev_values = Some(current_values);
    }
}

fn compute_dense_rank(
    rows: &[Row],
    schema: &TableSchema,
    wf: &WindowFuncInfo,
    row_indices: &[usize],
    wf_idx: usize,
    results: &mut [Vec<Value>],
) {
    let mut current_rank = 1i64;
    let mut prev_values: Option<Vec<Value>> = None;
    for &row_idx in row_indices {
        let current_values: Vec<Value> = wf
            .order_by
            .iter()
            .map(|o| eval_expr(&o.expr, Some(&rows[row_idx]), Some(schema)).unwrap_or(Value::Null))
            .collect();
        if let Some(prev) = &prev_values {
            if !order_by_values_are_peers(prev, &current_values, &wf.order_by) {
                current_rank += 1;
            }
        }
        results[row_idx][wf_idx] = Value::Int64(current_rank);
        prev_values = Some(current_values);
    }
}

fn order_by_values_are_peers(
    prev_values: &[Value],
    current_values: &[Value],
    order_by: &[OrderByExpr],
) -> bool {
    debug_assert_eq!(prev_values.len(), order_by.len());
    debug_assert_eq!(current_values.len(), order_by.len());

    for (order_expr, (prev_value, current_value)) in order_by
        .iter()
        .zip(prev_values.iter().zip(current_values.iter()))
    {
        let asc = order_expr.asc.unwrap_or(true);
        let nulls_first = order_expr.nulls_first.unwrap_or(!asc);
        if !matches!(
            compare_order_by_values(prev_value, current_value, asc, nulls_first),
            std::cmp::Ordering::Equal
        ) {
            return false;
        }
    }

    true
}

fn get_frame_bounds(
    wf: &WindowFuncInfo,
    current_pos: usize,
    partition_size: usize,
) -> (usize, usize) {
    let frame = match &wf.window_frame {
        Some(f) => f,
        None => {
            if wf.order_by.is_empty() {
                return (0, partition_size);
            } else {
                return (0, current_pos + 1);
            }
        }
    };

    let start = match &frame.start_bound {
        WindowFrameBound::CurrentRow => current_pos,
        WindowFrameBound::Preceding(None) => 0,
        WindowFrameBound::Preceding(Some(n)) => {
            if let Expr::Value(SqlValue::Number(s, _)) = n.as_ref() {
                let offset = s.parse::<usize>().unwrap_or(0);
                current_pos.saturating_sub(offset)
            } else {
                0
            }
        }
        WindowFrameBound::Following(None) => partition_size,
        WindowFrameBound::Following(Some(n)) => {
            if let Expr::Value(SqlValue::Number(s, _)) = n.as_ref() {
                let offset = s.parse::<usize>().unwrap_or(0);
                (current_pos + offset).min(partition_size)
            } else {
                partition_size
            }
        }
    };

    let end = match &frame.end_bound {
        None => current_pos + 1,
        Some(WindowFrameBound::CurrentRow) => current_pos + 1,
        Some(WindowFrameBound::Preceding(None)) => 0,
        Some(WindowFrameBound::Preceding(Some(n))) => {
            if let Expr::Value(SqlValue::Number(s, _)) = n.as_ref() {
                let offset = s.parse::<usize>().unwrap_or(0);
                (current_pos + 1).saturating_sub(offset)
            } else {
                0
            }
        }
        Some(WindowFrameBound::Following(None)) => partition_size,
        Some(WindowFrameBound::Following(Some(n))) => {
            if let Expr::Value(SqlValue::Number(s, _)) = n.as_ref() {
                let offset = s.parse::<usize>().unwrap_or(0);
                (current_pos + 1 + offset).min(partition_size)
            } else {
                partition_size
            }
        }
    };

    (start, end)
}

fn compute_sum(
    rows: &[Row],
    schema: &TableSchema,
    wf: &WindowFuncInfo,
    row_indices: &[usize],
    wf_idx: usize,
    results: &mut [Vec<Value>],
) -> Result<()> {
    let partition_size = row_indices.len();

    for (pos, &row_idx) in row_indices.iter().enumerate() {
        let (start, end) = get_frame_bounds(wf, pos, partition_size);
        let mut sum = 0.0f64;
        for i in start..end {
            if i < partition_size {
                let frame_row_idx = row_indices[i];
                if let Some(ref arg) = wf.arg_expr {
                    let val = eval_expr(arg, Some(&rows[frame_row_idx]), Some(schema))?;
                    match val {
                        Value::Int32(n) => sum += n as f64,
                        Value::Int64(n) => sum += n as f64,
                        Value::Float64(n) => sum += n,
                        Value::Numeric(d) => sum += d.to_f64().unwrap_or(0.0),
                        _ => {}
                    }
                }
            }
        }
        results[row_idx][wf_idx] = Value::Float64(sum);
    }
    Ok(())
}

fn compute_count(
    wf: &WindowFuncInfo,
    row_indices: &[usize],
    wf_idx: usize,
    results: &mut [Vec<Value>],
) {
    let partition_size = row_indices.len();

    for (pos, &row_idx) in row_indices.iter().enumerate() {
        let (start, end) = get_frame_bounds(wf, pos, partition_size);
        let count = (end.saturating_sub(start)).min(partition_size - start) as i64;
        results[row_idx][wf_idx] = Value::Int64(count);
    }
}

fn compute_avg(
    rows: &[Row],
    schema: &TableSchema,
    wf: &WindowFuncInfo,
    row_indices: &[usize],
    wf_idx: usize,
    results: &mut [Vec<Value>],
) -> Result<()> {
    let partition_size = row_indices.len();

    for (pos, &row_idx) in row_indices.iter().enumerate() {
        let (start, end) = get_frame_bounds(wf, pos, partition_size);
        let mut frame_sum = 0.0f64;
        let mut frame_count = 0i64;
        for i in start..end {
            if i < partition_size {
                let frame_row_idx = row_indices[i];
                if let Some(ref arg) = wf.arg_expr {
                    let val = eval_expr(arg, Some(&rows[frame_row_idx]), Some(schema))?;
                    match val {
                        Value::Int32(n) => {
                            frame_sum += n as f64;
                            frame_count += 1;
                        }
                        Value::Int64(n) => {
                            frame_sum += n as f64;
                            frame_count += 1;
                        }
                        Value::Float64(n) => {
                            frame_sum += n;
                            frame_count += 1;
                        }
                        Value::Numeric(d) => {
                            frame_sum += d.to_f64().unwrap_or(0.0);
                            frame_count += 1;
                        }
                        Value::Null => {}
                        _ => {
                            frame_count += 1;
                        }
                    }
                }
            }
        }
        results[row_idx][wf_idx] = if frame_count > 0 {
            Value::Float64(frame_sum / frame_count as f64)
        } else {
            Value::Null
        };
    }
    Ok(())
}

fn compute_min(
    rows: &[Row],
    schema: &TableSchema,
    wf: &WindowFuncInfo,
    row_indices: &[usize],
    wf_idx: usize,
    results: &mut [Vec<Value>],
) -> Result<()> {
    let partition_size = row_indices.len();

    for (pos, &row_idx) in row_indices.iter().enumerate() {
        let (start, end) = get_frame_bounds(wf, pos, partition_size);
        let mut min_val: Option<Value> = None;
        for i in start..end {
            if i < partition_size {
                let frame_row_idx = row_indices[i];
                if let Some(ref arg) = wf.arg_expr {
                    let val = eval_expr(arg, Some(&rows[frame_row_idx]), Some(schema))?;
                    if !matches!(val, Value::Null) {
                        min_val = Some(match &min_val {
                            None => val.clone(),
                            Some(m) => {
                                if compare_values(&val, m).unwrap_or(0) < 0 {
                                    val.clone()
                                } else {
                                    m.clone()
                                }
                            }
                        });
                    }
                }
            }
        }
        results[row_idx][wf_idx] = min_val.unwrap_or(Value::Null);
    }
    Ok(())
}

fn compute_max(
    rows: &[Row],
    schema: &TableSchema,
    wf: &WindowFuncInfo,
    row_indices: &[usize],
    wf_idx: usize,
    results: &mut [Vec<Value>],
) -> Result<()> {
    let partition_size = row_indices.len();

    for (pos, &row_idx) in row_indices.iter().enumerate() {
        let (start, end) = get_frame_bounds(wf, pos, partition_size);
        let mut max_val: Option<Value> = None;
        for i in start..end {
            if i < partition_size {
                let frame_row_idx = row_indices[i];
                if let Some(ref arg) = wf.arg_expr {
                    let val = eval_expr(arg, Some(&rows[frame_row_idx]), Some(schema))?;
                    if !matches!(val, Value::Null) {
                        max_val = Some(match &max_val {
                            None => val.clone(),
                            Some(m) => {
                                if compare_values(&val, m).unwrap_or(0) > 0 {
                                    val.clone()
                                } else {
                                    m.clone()
                                }
                            }
                        });
                    }
                }
            }
        }
        results[row_idx][wf_idx] = max_val.unwrap_or(Value::Null);
    }
    Ok(())
}

fn compute_lag(
    rows: &[Row],
    schema: &TableSchema,
    wf: &WindowFuncInfo,
    row_indices: &[usize],
    wf_idx: usize,
    results: &mut [Vec<Value>],
) -> Result<()> {
    let offset = wf
        .offset_expr
        .as_ref()
        .and_then(|e| match e {
            Expr::Value(SqlValue::Number(n, _)) => n.parse::<usize>().ok(),
            _ => None,
        })
        .unwrap_or(1);
    let default_val = wf
        .default_value_expr
        .as_ref()
        .map(|e| eval_expr(e, None, None).unwrap_or(Value::Null))
        .unwrap_or(Value::Null);

    for (pos, &row_idx) in row_indices.iter().enumerate() {
        let val = if pos >= offset {
            let lag_row_idx = row_indices[pos - offset];
            if let Some(ref arg) = wf.arg_expr {
                eval_expr(arg, Some(&rows[lag_row_idx]), Some(schema))?
            } else {
                Value::Null
            }
        } else {
            default_val.clone()
        };
        results[row_idx][wf_idx] = val;
    }
    Ok(())
}

fn compute_lead(
    rows: &[Row],
    schema: &TableSchema,
    wf: &WindowFuncInfo,
    row_indices: &[usize],
    wf_idx: usize,
    results: &mut [Vec<Value>],
) -> Result<()> {
    let offset = wf
        .offset_expr
        .as_ref()
        .and_then(|e| match e {
            Expr::Value(SqlValue::Number(n, _)) => n.parse::<usize>().ok(),
            _ => None,
        })
        .unwrap_or(1);
    let default_val = wf
        .default_value_expr
        .as_ref()
        .map(|e| eval_expr(e, None, None).unwrap_or(Value::Null))
        .unwrap_or(Value::Null);

    for (pos, &row_idx) in row_indices.iter().enumerate() {
        let val = if pos + offset < row_indices.len() {
            let lead_row_idx = row_indices[pos + offset];
            if let Some(ref arg) = wf.arg_expr {
                eval_expr(arg, Some(&rows[lead_row_idx]), Some(schema))?
            } else {
                Value::Null
            }
        } else {
            default_val.clone()
        };
        results[row_idx][wf_idx] = val;
    }
    Ok(())
}

fn compute_first_value(
    rows: &[Row],
    schema: &TableSchema,
    wf: &WindowFuncInfo,
    row_indices: &[usize],
    wf_idx: usize,
    results: &mut [Vec<Value>],
) -> Result<()> {
    let partition_size = row_indices.len();

    for (pos, &row_idx) in row_indices.iter().enumerate() {
        let (start, _end) = get_frame_bounds(wf, pos, partition_size);
        let val = if start < partition_size {
            let first_row_idx = row_indices[start];
            if let Some(ref arg) = wf.arg_expr {
                eval_expr(arg, Some(&rows[first_row_idx]), Some(schema))?
            } else {
                Value::Null
            }
        } else {
            Value::Null
        };
        results[row_idx][wf_idx] = val;
    }
    Ok(())
}

fn compute_last_value(
    rows: &[Row],
    schema: &TableSchema,
    wf: &WindowFuncInfo,
    row_indices: &[usize],
    wf_idx: usize,
    results: &mut [Vec<Value>],
) -> Result<()> {
    let partition_size = row_indices.len();

    for (pos, &row_idx) in row_indices.iter().enumerate() {
        let (_start, end) = get_frame_bounds(wf, pos, partition_size);
        let val = if end > 0 && end <= partition_size {
            let last_row_idx = row_indices[end - 1];
            if let Some(ref arg) = wf.arg_expr {
                eval_expr(arg, Some(&rows[last_row_idx]), Some(schema))?
            } else {
                Value::Null
            }
        } else {
            Value::Null
        };
        results[row_idx][wf_idx] = val;
    }
    Ok(())
}

/// Compute window functions for JOIN context
pub(crate) fn compute_window_functions_join(
    rows: &[Row],
    column_offsets: &HashMap<String, usize>,
    merged_column_offsets: Option<&HashMap<String, Vec<usize>>>,
    combined_schema: &TableSchema,
    window_funcs: &[WindowFuncInfo],
) -> Result<Vec<Vec<Value>>> {
    let mut results: Vec<Vec<Value>> = vec![vec![Value::Null; window_funcs.len()]; rows.len()];

    for (wf_idx, wf) in window_funcs.iter().enumerate() {
        // Build partitions
        let mut partitions: HashMap<Vec<u8>, Vec<usize>> = HashMap::new();
        for (row_idx, row) in rows.iter().enumerate() {
            let ctx = JoinContext {
                tables: HashMap::new(),
                column_offsets,
                merged_column_offsets,
                combined_row: row,
                combined_schema,
            };
            let mut key = Vec::new();
            for expr in &wf.partition_by {
                key.push(eval_expr_join(expr, &ctx)?);
            }
            let key_bytes = serialize_values_for_key(&key).unwrap_or_default();
            partitions.entry(key_bytes).or_default().push(row_idx);
        }

        for (_partition_key, mut row_indices) in partitions {
            // Sort within partition if ORDER BY is specified
            if !wf.order_by.is_empty() {
                row_indices.sort_by(|&a, &b| {
                    for order_expr in &wf.order_by {
                        let ctx_a = JoinContext {
                            tables: HashMap::new(),
                            column_offsets,
                            merged_column_offsets,
                            combined_row: &rows[a],
                            combined_schema,
                        };
                        let ctx_b = JoinContext {
                            tables: HashMap::new(),
                            column_offsets,
                            merged_column_offsets,
                            combined_row: &rows[b],
                            combined_schema,
                        };
                        let val_a = eval_expr_join(&order_expr.expr, &ctx_a).unwrap_or(Value::Null);
                        let val_b = eval_expr_join(&order_expr.expr, &ctx_b).unwrap_or(Value::Null);
                        let asc = order_expr.asc.unwrap_or(true);
                        let nulls_first = order_expr.nulls_first.unwrap_or(!asc);
                        let ord = compare_order_by_values(&val_a, &val_b, asc, nulls_first);
                        if !matches!(ord, std::cmp::Ordering::Equal) {
                            return ord;
                        }
                    }
                    std::cmp::Ordering::Equal
                });
            }

            // Compute the window function for this partition
            match wf.func_name.as_str() {
                "row_number" => compute_row_number(&row_indices, wf_idx, &mut results),
                "rank" => compute_rank_join(
                    rows,
                    column_offsets,
                    merged_column_offsets,
                    combined_schema,
                    wf,
                    &row_indices,
                    wf_idx,
                    &mut results,
                ),
                "dense_rank" => compute_dense_rank_join(
                    rows,
                    column_offsets,
                    merged_column_offsets,
                    combined_schema,
                    wf,
                    &row_indices,
                    wf_idx,
                    &mut results,
                ),
                "sum" => compute_sum_join(
                    rows,
                    column_offsets,
                    merged_column_offsets,
                    combined_schema,
                    wf,
                    &row_indices,
                    wf_idx,
                    &mut results,
                )?,
                "count" => compute_count(&wf, &row_indices, wf_idx, &mut results),
                "avg" => compute_avg_join(
                    rows,
                    column_offsets,
                    merged_column_offsets,
                    combined_schema,
                    wf,
                    &row_indices,
                    wf_idx,
                    &mut results,
                )?,
                "min" => compute_min_join(
                    rows,
                    column_offsets,
                    merged_column_offsets,
                    combined_schema,
                    wf,
                    &row_indices,
                    wf_idx,
                    &mut results,
                )?,
                "max" => compute_max_join(
                    rows,
                    column_offsets,
                    merged_column_offsets,
                    combined_schema,
                    wf,
                    &row_indices,
                    wf_idx,
                    &mut results,
                )?,
                "lag" => compute_lag_join(
                    rows,
                    column_offsets,
                    merged_column_offsets,
                    combined_schema,
                    wf,
                    &row_indices,
                    wf_idx,
                    &mut results,
                )?,
                "lead" => compute_lead_join(
                    rows,
                    column_offsets,
                    merged_column_offsets,
                    combined_schema,
                    wf,
                    &row_indices,
                    wf_idx,
                    &mut results,
                )?,
                _ => return Err(anyhow!("Unsupported window function: {}", wf.func_name)),
            }
        }
    }

    Ok(results)
}

fn compute_rank_join(
    rows: &[Row],
    column_offsets: &HashMap<String, usize>,
    merged_column_offsets: Option<&HashMap<String, Vec<usize>>>,
    combined_schema: &TableSchema,
    wf: &WindowFuncInfo,
    row_indices: &[usize],
    wf_idx: usize,
    results: &mut [Vec<Value>],
) {
    let mut current_rank = 1i64;
    let mut prev_values: Option<Vec<Value>> = None;
    for (pos, &row_idx) in row_indices.iter().enumerate() {
        let ctx = JoinContext {
            tables: HashMap::new(),
            column_offsets,
            merged_column_offsets,
            combined_row: &rows[row_idx],
            combined_schema,
        };
        let current_values: Vec<Value> = wf
            .order_by
            .iter()
            .map(|o| eval_expr_join(&o.expr, &ctx).unwrap_or(Value::Null))
            .collect();
        if let Some(prev) = &prev_values {
            if !order_by_values_are_peers(prev, &current_values, &wf.order_by) {
                current_rank = (pos + 1) as i64;
            }
        }
        results[row_idx][wf_idx] = Value::Int64(current_rank);
        prev_values = Some(current_values);
    }
}

fn compute_dense_rank_join(
    rows: &[Row],
    column_offsets: &HashMap<String, usize>,
    merged_column_offsets: Option<&HashMap<String, Vec<usize>>>,
    combined_schema: &TableSchema,
    wf: &WindowFuncInfo,
    row_indices: &[usize],
    wf_idx: usize,
    results: &mut [Vec<Value>],
) {
    let mut current_rank = 1i64;
    let mut prev_values: Option<Vec<Value>> = None;
    for &row_idx in row_indices {
        let ctx = JoinContext {
            tables: HashMap::new(),
            column_offsets,
            merged_column_offsets,
            combined_row: &rows[row_idx],
            combined_schema,
        };
        let current_values: Vec<Value> = wf
            .order_by
            .iter()
            .map(|o| eval_expr_join(&o.expr, &ctx).unwrap_or(Value::Null))
            .collect();
        if let Some(prev) = &prev_values {
            if !order_by_values_are_peers(prev, &current_values, &wf.order_by) {
                current_rank += 1;
            }
        }
        results[row_idx][wf_idx] = Value::Int64(current_rank);
        prev_values = Some(current_values);
    }
}

fn compute_sum_join(
    rows: &[Row],
    column_offsets: &HashMap<String, usize>,
    merged_column_offsets: Option<&HashMap<String, Vec<usize>>>,
    combined_schema: &TableSchema,
    wf: &WindowFuncInfo,
    row_indices: &[usize],
    wf_idx: usize,
    results: &mut [Vec<Value>],
) -> Result<()> {
    let frame_full_partition = matches!(
        wf.window_frame.as_ref(),
        Some(WindowFrame {
            start_bound: WindowFrameBound::Preceding(None),
            end_bound: Some(WindowFrameBound::Following(None)),
            ..
        })
    );
    let frame_running_from_start = matches!(
        wf.window_frame.as_ref(),
        Some(WindowFrame {
            start_bound: WindowFrameBound::Preceding(None),
            end_bound: None | Some(WindowFrameBound::CurrentRow),
            ..
        })
    );
    let full_partition = (wf.window_frame.is_none() && wf.order_by.is_empty()) || frame_full_partition;
    let running = (wf.window_frame.is_none() && !wf.order_by.is_empty()) || frame_running_from_start;

    if full_partition {
        let mut total = 0.0f64;
        for &row_idx in row_indices {
            if let Some(ref arg) = wf.arg_expr {
                let ctx = JoinContext {
                    tables: HashMap::new(),
                    column_offsets,
                    merged_column_offsets,
                    combined_row: &rows[row_idx],
                    combined_schema,
                };
                let val = eval_expr_join(arg, &ctx)?;
                match val {
                    Value::Int32(n) => total += n as f64,
                    Value::Int64(n) => total += n as f64,
                    Value::Float64(n) => total += n,
                    Value::Numeric(d) => total += d.to_f64().unwrap_or(0.0),
                    _ => {}
                }
            }
        }
        for &row_idx in row_indices {
            results[row_idx][wf_idx] = Value::Float64(total);
        }
        return Ok(());
    }

    if running {
        let mut running_sum = 0.0f64;
        for &row_idx in row_indices {
            if let Some(ref arg) = wf.arg_expr {
                let ctx = JoinContext {
                    tables: HashMap::new(),
                    column_offsets,
                    merged_column_offsets,
                    combined_row: &rows[row_idx],
                    combined_schema,
                };
                let val = eval_expr_join(arg, &ctx)?;
                match val {
                    Value::Int32(n) => running_sum += n as f64,
                    Value::Int64(n) => running_sum += n as f64,
                    Value::Float64(n) => running_sum += n,
                    Value::Numeric(d) => running_sum += d.to_f64().unwrap_or(0.0),
                    _ => {}
                }
            }
            results[row_idx][wf_idx] = Value::Float64(running_sum);
        }
        return Ok(());
    }

    let partition_size = row_indices.len();

    for (pos, &row_idx) in row_indices.iter().enumerate() {
        let (start, end) = get_frame_bounds(wf, pos, partition_size);
        let mut sum = 0.0f64;
        for i in start..end {
            if i < partition_size {
                let frame_row_idx = row_indices[i];
                if let Some(ref arg) = wf.arg_expr {
                    let ctx = JoinContext {
                        tables: HashMap::new(),
                        column_offsets,
                        merged_column_offsets,
                        combined_row: &rows[frame_row_idx],
                        combined_schema,
                    };
                    let val = eval_expr_join(arg, &ctx)?;
                    match val {
                        Value::Int32(n) => sum += n as f64,
                        Value::Int64(n) => sum += n as f64,
                        Value::Float64(n) => sum += n,
                        Value::Numeric(d) => sum += d.to_f64().unwrap_or(0.0),
                        _ => {}
                    }
                }
            }
        }
        results[row_idx][wf_idx] = Value::Float64(sum);
    }
    Ok(())
}

fn compute_avg_join(
    rows: &[Row],
    column_offsets: &HashMap<String, usize>,
    merged_column_offsets: Option<&HashMap<String, Vec<usize>>>,
    combined_schema: &TableSchema,
    wf: &WindowFuncInfo,
    row_indices: &[usize],
    wf_idx: usize,
    results: &mut [Vec<Value>],
) -> Result<()> {
    let frame_full_partition = matches!(
        wf.window_frame.as_ref(),
        Some(WindowFrame {
            start_bound: WindowFrameBound::Preceding(None),
            end_bound: Some(WindowFrameBound::Following(None)),
            ..
        })
    );
    let frame_running_from_start = matches!(
        wf.window_frame.as_ref(),
        Some(WindowFrame {
            start_bound: WindowFrameBound::Preceding(None),
            end_bound: None | Some(WindowFrameBound::CurrentRow),
            ..
        })
    );
    let full_partition = (wf.window_frame.is_none() && wf.order_by.is_empty()) || frame_full_partition;
    let running = (wf.window_frame.is_none() && !wf.order_by.is_empty()) || frame_running_from_start;

    if full_partition {
        let mut total_sum = 0.0f64;
        let mut total_count = 0i64;
        for &row_idx in row_indices {
            if let Some(ref arg) = wf.arg_expr {
                let ctx = JoinContext {
                    tables: HashMap::new(),
                    column_offsets,
                    merged_column_offsets,
                    combined_row: &rows[row_idx],
                    combined_schema,
                };
                let val = eval_expr_join(arg, &ctx)?;
                match val {
                    Value::Int32(n) => {
                        total_sum += n as f64;
                        total_count += 1;
                    }
                    Value::Int64(n) => {
                        total_sum += n as f64;
                        total_count += 1;
                    }
                    Value::Float64(n) => {
                        total_sum += n;
                        total_count += 1;
                    }
                    Value::Numeric(d) => {
                        total_sum += d.to_f64().unwrap_or(0.0);
                        total_count += 1;
                    }
                    Value::Null => {}
                    _ => {
                        total_count += 1;
                    }
                }
            }
        }
        let avg_val = if total_count > 0 {
            Value::Float64(total_sum / total_count as f64)
        } else {
            Value::Null
        };
        for &row_idx in row_indices {
            results[row_idx][wf_idx] = avg_val.clone();
        }
        return Ok(());
    }

    if running {
        let mut running_sum = 0.0f64;
        let mut running_count = 0i64;
        for &row_idx in row_indices {
            if let Some(ref arg) = wf.arg_expr {
                let ctx = JoinContext {
                    tables: HashMap::new(),
                    column_offsets,
                    merged_column_offsets,
                    combined_row: &rows[row_idx],
                    combined_schema,
                };
                let val = eval_expr_join(arg, &ctx)?;
                match val {
                    Value::Int32(n) => {
                        running_sum += n as f64;
                        running_count += 1;
                    }
                    Value::Int64(n) => {
                        running_sum += n as f64;
                        running_count += 1;
                    }
                    Value::Float64(n) => {
                        running_sum += n;
                        running_count += 1;
                    }
                    Value::Numeric(d) => {
                        running_sum += d.to_f64().unwrap_or(0.0);
                        running_count += 1;
                    }
                    Value::Null => {}
                    _ => {
                        running_count += 1;
                    }
                }
            }
            results[row_idx][wf_idx] = if running_count > 0 {
                Value::Float64(running_sum / running_count as f64)
            } else {
                Value::Null
            };
        }
        return Ok(());
    }

    let partition_size = row_indices.len();

    for (pos, &row_idx) in row_indices.iter().enumerate() {
        let (start, end) = get_frame_bounds(wf, pos, partition_size);
        let mut frame_sum = 0.0f64;
        let mut frame_count = 0i64;
        for i in start..end {
            if i < partition_size {
                let frame_row_idx = row_indices[i];
                if let Some(ref arg) = wf.arg_expr {
                    let ctx = JoinContext {
                        tables: HashMap::new(),
                        column_offsets,
                        merged_column_offsets,
                        combined_row: &rows[frame_row_idx],
                        combined_schema,
                    };
                    let val = eval_expr_join(arg, &ctx)?;
                    match val {
                        Value::Int32(n) => {
                            frame_sum += n as f64;
                            frame_count += 1;
                        }
                        Value::Int64(n) => {
                            frame_sum += n as f64;
                            frame_count += 1;
                        }
                        Value::Float64(n) => {
                            frame_sum += n;
                            frame_count += 1;
                        }
                        Value::Numeric(d) => {
                            frame_sum += d.to_f64().unwrap_or(0.0);
                            frame_count += 1;
                        }
                        Value::Null => {}
                        _ => {
                            frame_count += 1;
                        }
                    }
                }
            }
        }
        results[row_idx][wf_idx] = if frame_count > 0 {
            Value::Float64(frame_sum / frame_count as f64)
        } else {
            Value::Null
        };
    }
    Ok(())
}

fn compute_min_join(
    rows: &[Row],
    column_offsets: &HashMap<String, usize>,
    merged_column_offsets: Option<&HashMap<String, Vec<usize>>>,
    combined_schema: &TableSchema,
    wf: &WindowFuncInfo,
    row_indices: &[usize],
    wf_idx: usize,
    results: &mut [Vec<Value>],
) -> Result<()> {
    let frame_full_partition = matches!(
        wf.window_frame.as_ref(),
        Some(WindowFrame {
            start_bound: WindowFrameBound::Preceding(None),
            end_bound: Some(WindowFrameBound::Following(None)),
            ..
        })
    );
    let frame_running_from_start = matches!(
        wf.window_frame.as_ref(),
        Some(WindowFrame {
            start_bound: WindowFrameBound::Preceding(None),
            end_bound: None | Some(WindowFrameBound::CurrentRow),
            ..
        })
    );
    let full_partition = (wf.window_frame.is_none() && wf.order_by.is_empty()) || frame_full_partition;
    let running = (wf.window_frame.is_none() && !wf.order_by.is_empty()) || frame_running_from_start;

    if full_partition {
        let mut min_val: Option<Value> = None;
        for &row_idx in row_indices {
            if let Some(ref arg) = wf.arg_expr {
                let ctx = JoinContext {
                    tables: HashMap::new(),
                    column_offsets,
                    merged_column_offsets,
                    combined_row: &rows[row_idx],
                    combined_schema,
                };
                let val = eval_expr_join(arg, &ctx)?;
                if !matches!(val, Value::Null) {
                    min_val = Some(match &min_val {
                        None => val.clone(),
                        Some(m) => {
                            if compare_values(&val, m).unwrap_or(0) < 0 {
                                val.clone()
                            } else {
                                m.clone()
                            }
                        }
                    });
                }
            }
        }
        let final_min = min_val.unwrap_or(Value::Null);
        for &row_idx in row_indices {
            results[row_idx][wf_idx] = final_min.clone();
        }
        return Ok(());
    }

    if running {
        let mut min_val: Option<Value> = None;
        for &row_idx in row_indices {
            if let Some(ref arg) = wf.arg_expr {
                let ctx = JoinContext {
                    tables: HashMap::new(),
                    column_offsets,
                    merged_column_offsets,
                    combined_row: &rows[row_idx],
                    combined_schema,
                };
                let val = eval_expr_join(arg, &ctx)?;
                if !matches!(val, Value::Null) {
                    min_val = Some(match &min_val {
                        None => val.clone(),
                        Some(m) => {
                            if compare_values(&val, m).unwrap_or(0) < 0 {
                                val.clone()
                            } else {
                                m.clone()
                            }
                        }
                    });
                }
            }
            results[row_idx][wf_idx] = min_val.clone().unwrap_or(Value::Null);
        }
        return Ok(());
    }

    let partition_size = row_indices.len();

    for (pos, &row_idx) in row_indices.iter().enumerate() {
        let (start, end) = get_frame_bounds(wf, pos, partition_size);
        let mut min_val: Option<Value> = None;
        for i in start..end {
            if i < partition_size {
                let frame_row_idx = row_indices[i];
                if let Some(ref arg) = wf.arg_expr {
                    let ctx = JoinContext {
                        tables: HashMap::new(),
                        column_offsets,
                        merged_column_offsets,
                        combined_row: &rows[frame_row_idx],
                        combined_schema,
                    };
                    let val = eval_expr_join(arg, &ctx)?;
                    if !matches!(val, Value::Null) {
                        min_val = Some(match &min_val {
                            None => val.clone(),
                            Some(m) => {
                                if compare_values(&val, m).unwrap_or(0) < 0 {
                                    val.clone()
                                } else {
                                    m.clone()
                                }
                            }
                        });
                    }
                }
            }
        }
        results[row_idx][wf_idx] = min_val.unwrap_or(Value::Null);
    }
    Ok(())
}

fn compute_max_join(
    rows: &[Row],
    column_offsets: &HashMap<String, usize>,
    merged_column_offsets: Option<&HashMap<String, Vec<usize>>>,
    combined_schema: &TableSchema,
    wf: &WindowFuncInfo,
    row_indices: &[usize],
    wf_idx: usize,
    results: &mut [Vec<Value>],
) -> Result<()> {
    let frame_full_partition = matches!(
        wf.window_frame.as_ref(),
        Some(WindowFrame {
            start_bound: WindowFrameBound::Preceding(None),
            end_bound: Some(WindowFrameBound::Following(None)),
            ..
        })
    );
    let frame_running_from_start = matches!(
        wf.window_frame.as_ref(),
        Some(WindowFrame {
            start_bound: WindowFrameBound::Preceding(None),
            end_bound: None | Some(WindowFrameBound::CurrentRow),
            ..
        })
    );
    let full_partition = (wf.window_frame.is_none() && wf.order_by.is_empty()) || frame_full_partition;
    let running = (wf.window_frame.is_none() && !wf.order_by.is_empty()) || frame_running_from_start;

    if full_partition {
        let mut max_val: Option<Value> = None;
        for &row_idx in row_indices {
            if let Some(ref arg) = wf.arg_expr {
                let ctx = JoinContext {
                    tables: HashMap::new(),
                    column_offsets,
                    merged_column_offsets,
                    combined_row: &rows[row_idx],
                    combined_schema,
                };
                let val = eval_expr_join(arg, &ctx)?;
                if !matches!(val, Value::Null) {
                    max_val = Some(match &max_val {
                        None => val.clone(),
                        Some(m) => {
                            if compare_values(&val, m).unwrap_or(0) > 0 {
                                val.clone()
                            } else {
                                m.clone()
                            }
                        }
                    });
                }
            }
        }
        let final_max = max_val.unwrap_or(Value::Null);
        for &row_idx in row_indices {
            results[row_idx][wf_idx] = final_max.clone();
        }
        return Ok(());
    }

    if running {
        let mut max_val: Option<Value> = None;
        for &row_idx in row_indices {
            if let Some(ref arg) = wf.arg_expr {
                let ctx = JoinContext {
                    tables: HashMap::new(),
                    column_offsets,
                    merged_column_offsets,
                    combined_row: &rows[row_idx],
                    combined_schema,
                };
                let val = eval_expr_join(arg, &ctx)?;
                if !matches!(val, Value::Null) {
                    max_val = Some(match &max_val {
                        None => val.clone(),
                        Some(m) => {
                            if compare_values(&val, m).unwrap_or(0) > 0 {
                                val.clone()
                            } else {
                                m.clone()
                            }
                        }
                    });
                }
            }
            results[row_idx][wf_idx] = max_val.clone().unwrap_or(Value::Null);
        }
        return Ok(());
    }

    let partition_size = row_indices.len();

    for (pos, &row_idx) in row_indices.iter().enumerate() {
        let (start, end) = get_frame_bounds(wf, pos, partition_size);
        let mut max_val: Option<Value> = None;
        for i in start..end {
            if i < partition_size {
                let frame_row_idx = row_indices[i];
                if let Some(ref arg) = wf.arg_expr {
                    let ctx = JoinContext {
                        tables: HashMap::new(),
                        column_offsets,
                        merged_column_offsets,
                        combined_row: &rows[frame_row_idx],
                        combined_schema,
                    };
                    let val = eval_expr_join(arg, &ctx)?;
                    if !matches!(val, Value::Null) {
                        max_val = Some(match &max_val {
                            None => val.clone(),
                            Some(m) => {
                                if compare_values(&val, m).unwrap_or(0) > 0 {
                                    val.clone()
                                } else {
                                    m.clone()
                                }
                            }
                        });
                    }
                }
            }
        }
        results[row_idx][wf_idx] = max_val.unwrap_or(Value::Null);
    }
    Ok(())
}

fn compute_lag_join(
    rows: &[Row],
    column_offsets: &HashMap<String, usize>,
    merged_column_offsets: Option<&HashMap<String, Vec<usize>>>,
    combined_schema: &TableSchema,
    wf: &WindowFuncInfo,
    row_indices: &[usize],
    wf_idx: usize,
    results: &mut [Vec<Value>],
) -> Result<()> {
    let offset = wf
        .offset_expr
        .as_ref()
        .and_then(|e| match e {
            Expr::Value(SqlValue::Number(n, _)) => n.parse::<usize>().ok(),
            _ => None,
        })
        .unwrap_or(1);
    let default_val = wf
        .default_value_expr
        .as_ref()
        .map(|e| eval_expr(e, None, None).unwrap_or(Value::Null))
        .unwrap_or(Value::Null);

    for (pos, &row_idx) in row_indices.iter().enumerate() {
        let val = if pos >= offset {
            let lag_row_idx = row_indices[pos - offset];
            if let Some(ref arg) = wf.arg_expr {
                let ctx = JoinContext {
                    tables: HashMap::new(),
                    column_offsets,
                    merged_column_offsets,
                    combined_row: &rows[lag_row_idx],
                    combined_schema,
                };
                eval_expr_join(arg, &ctx)?
            } else {
                Value::Null
            }
        } else {
            default_val.clone()
        };
        results[row_idx][wf_idx] = val;
    }
    Ok(())
}

fn compute_lead_join(
    rows: &[Row],
    column_offsets: &HashMap<String, usize>,
    merged_column_offsets: Option<&HashMap<String, Vec<usize>>>,
    combined_schema: &TableSchema,
    wf: &WindowFuncInfo,
    row_indices: &[usize],
    wf_idx: usize,
    results: &mut [Vec<Value>],
) -> Result<()> {
    let offset = wf
        .offset_expr
        .as_ref()
        .and_then(|e| match e {
            Expr::Value(SqlValue::Number(n, _)) => n.parse::<usize>().ok(),
            _ => None,
        })
        .unwrap_or(1);
    let default_val = wf
        .default_value_expr
        .as_ref()
        .map(|e| eval_expr(e, None, None).unwrap_or(Value::Null))
        .unwrap_or(Value::Null);

    for (pos, &row_idx) in row_indices.iter().enumerate() {
        let val = if pos + offset < row_indices.len() {
            let lead_row_idx = row_indices[pos + offset];
            if let Some(ref arg) = wf.arg_expr {
                let ctx = JoinContext {
                    tables: HashMap::new(),
                    column_offsets,
                    merged_column_offsets,
                    combined_row: &rows[lead_row_idx],
                    combined_schema,
                };
                eval_expr_join(arg, &ctx)?
            } else {
                Value::Null
            }
        } else {
            default_val.clone()
        };
        results[row_idx][wf_idx] = val;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ColumnDef, DataType};
    use sqlparser::ast::WindowFrameUnits;

    fn test_schema() -> TableSchema {
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
        }
    }

    fn row_number_partition_by_grp() -> WindowFuncInfo {
        WindowFuncInfo {
            proj_idx: 0,
            func_name: "row_number".to_string(),
            arg_expr: None,
            partition_by: vec![Expr::Identifier(sqlparser::ast::Ident::new("grp"))],
            order_by: vec![],
            offset_expr: None,
            default_value_expr: None,
            window_frame: None,
        }
    }

    fn rank_order_by_grp() -> WindowFuncInfo {
        WindowFuncInfo {
            proj_idx: 0,
            func_name: "rank".to_string(),
            arg_expr: None,
            partition_by: vec![],
            order_by: vec![OrderByExpr {
                expr: Expr::Identifier(sqlparser::ast::Ident::new("grp")),
                asc: Some(true),
                nulls_first: None,
            }],
            offset_expr: None,
            default_value_expr: None,
            window_frame: None,
        }
    }

    fn dense_rank_order_by_grp() -> WindowFuncInfo {
        WindowFuncInfo {
            proj_idx: 0,
            func_name: "dense_rank".to_string(),
            arg_expr: None,
            partition_by: vec![],
            order_by: vec![OrderByExpr {
                expr: Expr::Identifier(sqlparser::ast::Ident::new("grp")),
                asc: Some(true),
                nulls_first: None,
            }],
            offset_expr: None,
            default_value_expr: None,
            window_frame: None,
        }
    }

    #[test]
    fn window_partitions_canonicalize_float_keys() {
        let schema = test_schema();

        let window_funcs = vec![row_number_partition_by_grp()];
        let rows = vec![
            Row::new(vec![Value::Int32(1), Value::Float64(-0.0)]),
            Row::new(vec![Value::Int32(2), Value::Float64(0.0)]),
        ];
        let results = compute_window_functions(&rows, &schema, &window_funcs).unwrap();
        assert_eq!(results[0][0], Value::Int64(1));
        assert_eq!(results[1][0], Value::Int64(2));

        let nan1 = f64::from_bits(0x7ff8_0000_0000_0001);
        let nan2 = f64::from_bits(0x7ff8_0000_0000_0002);
        assert!(nan1.is_nan() && nan2.is_nan());
        let rows = vec![
            Row::new(vec![Value::Int32(1), Value::Float64(nan1)]),
            Row::new(vec![Value::Int32(2), Value::Float64(nan2)]),
        ];
        let results = compute_window_functions(&rows, &schema, &window_funcs).unwrap();
        assert_eq!(results[0][0], Value::Int64(1));
        assert_eq!(results[1][0], Value::Int64(2));
    }

    #[test]
    fn window_rank_dense_rank_treat_nan_order_keys_as_peers() {
        let schema = test_schema();
        let window_funcs = vec![rank_order_by_grp(), dense_rank_order_by_grp()];

        let nan1 = f64::from_bits(0x7ff8_0000_0000_0001);
        let nan2 = f64::from_bits(0x7ff8_0000_0000_0002);
        assert!(nan1.is_nan() && nan2.is_nan());

        let rows = vec![
            Row::new(vec![Value::Int32(1), Value::Float64(nan1)]),
            Row::new(vec![Value::Int32(2), Value::Float64(nan2)]),
            Row::new(vec![Value::Int32(3), Value::Float64(1.0)]),
            Row::new(vec![Value::Int32(4), Value::Float64(2.0)]),
        ];

        let results = compute_window_functions(&rows, &schema, &window_funcs).unwrap();

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
    fn window_rank_dense_rank_join_treat_nan_order_keys_as_peers() {
        let schema = test_schema();
        let window_funcs = vec![rank_order_by_grp(), dense_rank_order_by_grp()];

        let mut column_offsets = HashMap::new();
        column_offsets.insert("id".to_string(), 0);
        column_offsets.insert("grp".to_string(), 1);

        let nan1 = f64::from_bits(0x7ff8_0000_0000_0001);
        let nan2 = f64::from_bits(0x7ff8_0000_0000_0002);
        assert!(nan1.is_nan() && nan2.is_nan());

        let rows = vec![
            Row::new(vec![Value::Int32(1), Value::Float64(nan1)]),
            Row::new(vec![Value::Int32(2), Value::Float64(nan2)]),
            Row::new(vec![Value::Int32(3), Value::Float64(1.0)]),
            Row::new(vec![Value::Int32(4), Value::Float64(2.0)]),
        ];

        let results = compute_window_functions_join(
            &rows,
            &column_offsets,
            None,
            &schema,
            &window_funcs,
        )
        .unwrap();

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
    fn window_join_aggregates_respect_explicit_frame() {
        let schema = test_schema();
        let window_frame = Some(WindowFrame {
            units: WindowFrameUnits::Rows,
            start_bound: WindowFrameBound::Preceding(None),
            end_bound: Some(WindowFrameBound::Following(None)),
        });

        let window_func = |func_name: &str, proj_idx: usize| WindowFuncInfo {
            proj_idx,
            func_name: func_name.to_string(),
            arg_expr: Some(Expr::Identifier(sqlparser::ast::Ident::new("grp"))),
            partition_by: vec![],
            order_by: vec![OrderByExpr {
                expr: Expr::Identifier(sqlparser::ast::Ident::new("id")),
                asc: Some(true),
                nulls_first: None,
            }],
            offset_expr: None,
            default_value_expr: None,
            window_frame: window_frame.clone(),
        };

        let window_funcs = vec![
            window_func("sum", 0),
            window_func("avg", 1),
            window_func("min", 2),
            window_func("max", 3),
        ];

        let mut column_offsets = HashMap::new();
        column_offsets.insert("id".to_string(), 0);
        column_offsets.insert("grp".to_string(), 1);

        let rows = vec![
            Row::new(vec![Value::Int32(1), Value::Float64(20.0)]),
            Row::new(vec![Value::Int32(2), Value::Float64(10.0)]),
            Row::new(vec![Value::Int32(3), Value::Float64(30.0)]),
        ];

        let results = compute_window_functions_join(
            &rows,
            &column_offsets,
            None,
            &schema,
            &window_funcs,
        )
        .unwrap();

        for row in &results {
            assert_eq!(row[0], Value::Float64(60.0));
            assert_eq!(row[1], Value::Float64(20.0));
            assert_eq!(row[2], Value::Float64(10.0));
            assert_eq!(row[3], Value::Float64(30.0));
        }
    }
}
