//! Window function operator for Volcano-style execution
//!
//! This operator computes window functions over partitioned and ordered data.

use std::collections::HashMap;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use sqlparser::ast::{Expr, OrderByExpr, WindowFrame, WindowFrameBound};

use super::{collect_all, BoxedOperator, ExecutionContext, PhysicalOperator};
use crate::sql::expr::{compare_order_by_values, eval_expr};
use crate::types::{ColumnDef, DataType, Row, TableSchema, Value};

/// Information about a single window function in the projection.
#[derive(Debug, Clone)]
pub struct WindowFunctionExpr {
    /// Name of the window function (e.g., "row_number", "sum", "lag")
    pub func_name: String,
    /// Argument expression (if any)
    pub arg_expr: Option<Expr>,
    /// PARTITION BY expressions
    pub partition_by: Vec<Expr>,
    /// ORDER BY expressions within the window
    pub order_by: Vec<OrderByExpr>,
    /// Offset expression for LAG/LEAD
    pub offset_expr: Option<Expr>,
    /// Default value expression for LAG/LEAD
    pub default_value_expr: Option<Expr>,
    /// Window frame specification
    pub window_frame: Option<WindowFrame>,
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

    fn compute_window_functions(&self, rows: &[Row], schema: &TableSchema) -> Result<Vec<Vec<Value>>> {
        let num_funcs = self.window_functions.len();
        let mut results: Vec<Vec<Value>> = vec![vec![Value::Null; num_funcs]; rows.len()];

        for (wf_idx, wf) in self.window_functions.iter().enumerate() {
            // Partition rows
            let mut partitions: HashMap<Vec<u8>, Vec<usize>> = HashMap::new();
            for (row_idx, row) in rows.iter().enumerate() {
                let mut key = Vec::new();
                for expr in &wf.partition_by {
                    key.push(eval_expr(expr, Some(row), Some(schema))?);
                }
                let key_bytes = bincode::serialize(&key).unwrap_or_default();
                partitions.entry(key_bytes).or_default().push(row_idx);
            }

            // Process each partition
            for (_partition_key, mut row_indices) in partitions {
                // Sort within partition if ORDER BY specified
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

                // Compute function for this partition
                match wf.func_name.as_str() {
                    "row_number" => self.compute_row_number(&row_indices, wf_idx, &mut results),
                    "rank" => self.compute_rank(rows, schema, wf, &row_indices, wf_idx, &mut results),
                    "dense_rank" => self.compute_dense_rank(rows, schema, wf, &row_indices, wf_idx, &mut results),
                    "sum" => self.compute_sum(rows, schema, wf, &row_indices, wf_idx, &mut results)?,
                    "count" => self.compute_count(wf, &row_indices, wf_idx, &mut results),
                    "avg" => self.compute_avg(rows, schema, wf, &row_indices, wf_idx, &mut results)?,
                    "min" => self.compute_min(rows, schema, wf, &row_indices, wf_idx, &mut results)?,
                    "max" => self.compute_max(rows, schema, wf, &row_indices, wf_idx, &mut results)?,
                    "lag" => self.compute_lag(rows, schema, wf, &row_indices, wf_idx, &mut results)?,
                    "lead" => self.compute_lead(rows, schema, wf, &row_indices, wf_idx, &mut results)?,
                    "first_value" => self.compute_first_value(rows, schema, wf, &row_indices, wf_idx, &mut results)?,
                    "last_value" => self.compute_last_value(rows, schema, wf, &row_indices, wf_idx, &mut results)?,
                    _ => return Err(anyhow!("Unsupported window function: {}", wf.func_name)),
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
        rows: &[Row],
        schema: &TableSchema,
        wf: &WindowFunctionExpr,
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
                if prev != &current_values {
                    current_rank = (pos + 1) as i64;
                }
            }
            results[row_idx][wf_idx] = Value::Int64(current_rank);
            prev_values = Some(current_values);
        }
    }

    fn compute_dense_rank(
        &self,
        rows: &[Row],
        schema: &TableSchema,
        wf: &WindowFunctionExpr,
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
                if prev != &current_values {
                    current_rank += 1;
                }
            }
            results[row_idx][wf_idx] = Value::Int64(current_rank);
            prev_values = Some(current_values);
        }
    }

    fn get_frame_bounds(
        &self,
        wf: &WindowFunctionExpr,
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
            WindowFrameBound::Preceding(Some(n)) => {
                let offset = self.eval_frame_bound_offset(n);
                current_pos.saturating_sub(offset)
            }
            WindowFrameBound::Preceding(None) => 0, // UNBOUNDED PRECEDING
            WindowFrameBound::CurrentRow => current_pos,
            WindowFrameBound::Following(Some(n)) => {
                let offset = self.eval_frame_bound_offset(n);
                current_pos.saturating_add(offset).min(partition_size)
            }
            WindowFrameBound::Following(None) => partition_size, // UNBOUNDED FOLLOWING
        };

        let end = match &frame.end_bound {
            Some(bound) => match bound {
                WindowFrameBound::Preceding(Some(n)) => {
                    let offset = self.eval_frame_bound_offset(n);
                    current_pos.saturating_add(1).saturating_sub(offset)
                }
                WindowFrameBound::Preceding(None) => 0,
                WindowFrameBound::CurrentRow => current_pos + 1,
                WindowFrameBound::Following(Some(n)) => {
                    let offset = self.eval_frame_bound_offset(n);
                    current_pos
                        .saturating_add(1)
                        .saturating_add(offset)
                        .min(partition_size)
                }
                WindowFrameBound::Following(None) => partition_size,
            },
            None => current_pos + 1, // Default to CURRENT ROW
        };

        (start, end)
    }

    fn eval_frame_bound_offset(&self, expr: &Expr) -> usize {
        match eval_expr(expr, None, None) {
            Ok(Value::Int32(n)) if n >= 0 => n as usize,
            Ok(Value::Int64(n)) if n >= 0 => n as usize,
            _ => 0,
        }
    }

    fn compute_sum(
        &self,
        rows: &[Row],
        schema: &TableSchema,
        wf: &WindowFunctionExpr,
        row_indices: &[usize],
        wf_idx: usize,
        results: &mut [Vec<Value>],
    ) -> Result<()> {
        let partition_size = row_indices.len();
        for (pos, &row_idx) in row_indices.iter().enumerate() {
            let (start, end) = self.get_frame_bounds(wf, pos, partition_size);
            let mut sum = rust_decimal::Decimal::ZERO;
            let mut has_value = false;
            for i in start..end {
                if let Some(expr) = &wf.arg_expr {
                    let val = eval_expr(expr, Some(&rows[row_indices[i]]), Some(schema))?;
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
        wf: &WindowFunctionExpr,
        row_indices: &[usize],
        wf_idx: usize,
        results: &mut [Vec<Value>],
    ) {
        let partition_size = row_indices.len();
        for (pos, &row_idx) in row_indices.iter().enumerate() {
            let (start, end) = self.get_frame_bounds(wf, pos, partition_size);
            let count = end
                .saturating_sub(start)
                .min(partition_size.saturating_sub(start)) as i64;
            results[row_idx][wf_idx] = Value::Int64(count);
        }
    }

    fn compute_avg(
        &self,
        rows: &[Row],
        schema: &TableSchema,
        wf: &WindowFunctionExpr,
        row_indices: &[usize],
        wf_idx: usize,
        results: &mut [Vec<Value>],
    ) -> Result<()> {
        let partition_size = row_indices.len();
        for (pos, &row_idx) in row_indices.iter().enumerate() {
            let (start, end) = self.get_frame_bounds(wf, pos, partition_size);
            let mut sum = rust_decimal::Decimal::ZERO;
            let mut count = 0i64;
            for i in start..end {
                if let Some(expr) = &wf.arg_expr {
                    let val = eval_expr(expr, Some(&rows[row_indices[i]]), Some(schema))?;
                    if let Some(n) = self.value_to_decimal(&val) {
                        sum += n;
                        count += 1;
                    }
                }
            }
            results[row_idx][wf_idx] = if count > 0 {
                Value::Numeric(sum / rust_decimal::Decimal::from(count))
            } else {
                Value::Null
            };
        }
        Ok(())
    }

    fn compute_min(
        &self,
        rows: &[Row],
        schema: &TableSchema,
        wf: &WindowFunctionExpr,
        row_indices: &[usize],
        wf_idx: usize,
        results: &mut [Vec<Value>],
    ) -> Result<()> {
        let partition_size = row_indices.len();
        for (pos, &row_idx) in row_indices.iter().enumerate() {
            let (start, end) = self.get_frame_bounds(wf, pos, partition_size);
            let mut min_val: Option<Value> = None;
            for i in start..end {
                if let Some(expr) = &wf.arg_expr {
                    let val = eval_expr(expr, Some(&rows[row_indices[i]]), Some(schema))?;
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
        schema: &TableSchema,
        wf: &WindowFunctionExpr,
        row_indices: &[usize],
        wf_idx: usize,
        results: &mut [Vec<Value>],
    ) -> Result<()> {
        let partition_size = row_indices.len();
        for (pos, &row_idx) in row_indices.iter().enumerate() {
            let (start, end) = self.get_frame_bounds(wf, pos, partition_size);
            let mut max_val: Option<Value> = None;
            for i in start..end {
                if let Some(expr) = &wf.arg_expr {
                    let val = eval_expr(expr, Some(&rows[row_indices[i]]), Some(schema))?;
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
        schema: &TableSchema,
        wf: &WindowFunctionExpr,
        row_indices: &[usize],
        wf_idx: usize,
        results: &mut [Vec<Value>],
    ) -> Result<()> {
        let offset = match wf.offset_expr.as_ref() {
            None => 1,
            Some(expr) => match eval_expr(expr, None, None) {
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
                    usize::try_from(n)
                        .map_err(|_| anyhow!("LAG offset too large: {}", n))?
                }
                _ => 1,
            },
        };

        let default_value = wf
            .default_value_expr
            .as_ref()
            .map(|e| eval_expr(e, None, None).unwrap_or(Value::Null))
            .unwrap_or(Value::Null);

        for (pos, &row_idx) in row_indices.iter().enumerate() {
            results[row_idx][wf_idx] = if pos >= offset {
                if let Some(expr) = &wf.arg_expr {
                    eval_expr(expr, Some(&rows[row_indices[pos - offset]]), Some(schema))?
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
        schema: &TableSchema,
        wf: &WindowFunctionExpr,
        row_indices: &[usize],
        wf_idx: usize,
        results: &mut [Vec<Value>],
    ) -> Result<()> {
        let offset = match wf.offset_expr.as_ref() {
            None => 1,
            Some(expr) => match eval_expr(expr, None, None) {
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
                    usize::try_from(n)
                        .map_err(|_| anyhow!("LEAD offset too large: {}", n))?
                }
                _ => 1,
            },
        };

        let default_value = wf
            .default_value_expr
            .as_ref()
            .map(|e| eval_expr(e, None, None).unwrap_or(Value::Null))
            .unwrap_or(Value::Null);

        let partition_size = row_indices.len();
        for (pos, &row_idx) in row_indices.iter().enumerate() {
            results[row_idx][wf_idx] = if pos + offset < partition_size {
                if let Some(expr) = &wf.arg_expr {
                    eval_expr(expr, Some(&rows[row_indices[pos + offset]]), Some(schema))?
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
        schema: &TableSchema,
        wf: &WindowFunctionExpr,
        row_indices: &[usize],
        wf_idx: usize,
        results: &mut [Vec<Value>],
    ) -> Result<()> {
        let partition_size = row_indices.len();
        for (pos, &row_idx) in row_indices.iter().enumerate() {
            let (start, end) = self.get_frame_bounds(wf, pos, partition_size);
            results[row_idx][wf_idx] = if start < end && start < partition_size {
                if let Some(expr) = &wf.arg_expr {
                    eval_expr(expr, Some(&rows[row_indices[start]]), Some(schema))?
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
        schema: &TableSchema,
        wf: &WindowFunctionExpr,
        row_indices: &[usize],
        wf_idx: usize,
        results: &mut [Vec<Value>],
    ) -> Result<()> {
        let partition_size = row_indices.len();
        for (pos, &row_idx) in row_indices.iter().enumerate() {
            let (start, end) = self.get_frame_bounds(wf, pos, partition_size);
            results[row_idx][wf_idx] = if start < end && end > 0 && end <= partition_size {
                if let Some(expr) = &wf.arg_expr {
                    eval_expr(expr, Some(&rows[row_indices[end - 1]]), Some(schema))?
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
        let input_schema = self.child.schema();

        // Compute window function values
        let window_results = self.compute_window_functions(&input_rows, input_schema)?;

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
}
