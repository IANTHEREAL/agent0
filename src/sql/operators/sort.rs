use anyhow::{anyhow, Result};
use async_trait::async_trait;
use sqlparser::ast::OrderByExpr;

use super::{BoxedOperator, ExecutionContext, PhysicalOperator};
use crate::sql::expr::{compare_order_by_values, eval_expr};
use crate::sql::sequences;
use crate::types::{Row, TableSchema, Value};

fn estimated_value_size(value: &Value) -> usize {
    match value {
        Value::Null | Value::Boolean(_) => 0,
        Value::Int32(_) => 4,
        Value::Int64(_) => 8,
        Value::Float64(_) => 8,
        Value::Text(s) => s.len(),
        Value::Bytes(b) => b.len(),
        Value::Timestamp(_) => 8,
        Value::Interval(_) => std::mem::size_of::<crate::types::IntervalValue>(),
        Value::Uuid(_) => 16,
        Value::Array(arr) => {
            std::mem::size_of::<Vec<Value>>()
                + arr.iter().map(estimated_value_size).sum::<usize>()
                + arr.len() * std::mem::size_of::<Value>()
        }
        Value::Vector(vec) => {
            std::mem::size_of::<Vec<f64>>() + vec.len() * std::mem::size_of::<f64>()
        }
        Value::Json(s) | Value::Jsonb(s) => s.len(),
        Value::Time(_) => 8,
        Value::Date(_) => 4,
        Value::Numeric(_) => 16,
        Value::Tsvector(s) | Value::Tsquery(s) => s.len(),
    }
}

fn estimated_row_size(row: &Row) -> usize {
    std::mem::size_of::<Row>()
        + std::mem::size_of::<Vec<Value>>()
        + row.values.len() * std::mem::size_of::<Value>()
        + row.values.iter().map(estimated_value_size).sum::<usize>()
}

fn enforce_sort_memory_limit(
    total_bytes: &mut usize,
    row: &Row,
    max_sort_bytes: usize,
) -> Result<()> {
    if max_sort_bytes == 0 {
        return Ok(());
    }

    *total_bytes = total_bytes.saturating_add(estimated_row_size(row));
    if *total_bytes > max_sort_bytes {
        return Err(anyhow!(
            "ORDER BY sort memory limit exceeded: estimated {} bytes exceeds pgtikv.max_sort_bytes={} bytes. Reduce result set with WHERE/LIMIT or increase pgtikv.max_sort_bytes",
            *total_bytes,
            max_sort_bytes
        ));
    }

    Ok(())
}

#[derive(Debug)]
pub struct SortOperator {
    child: BoxedOperator,
    order_by: Vec<OrderByExpr>,
    sorted_rows: Vec<Row>,
    position: usize,
    opened: bool,
}

impl SortOperator {
    pub fn new(child: BoxedOperator, order_by: Vec<OrderByExpr>) -> Self {
        Self {
            child,
            order_by,
            sorted_rows: Vec::new(),
            position: 0,
            opened: false,
        }
    }

    async fn compute_sort_keys(
        &self,
        row: &Row,
        ctx: &mut ExecutionContext<'_>,
    ) -> Result<Vec<Value>> {
        let schema = self.child.schema();
        let mut keys = Vec::with_capacity(self.order_by.len());
        for order_expr in &self.order_by {
            let value = if sequences::expr_needs_async_eval(&order_expr.expr) {
                sequences::eval_expr_with_sequences(
                    &ctx.store,
                    ctx.txn,
                    ctx.db_id,
                    ctx.sequence_values,
                    ctx.search_path,
                    &order_expr.expr,
                    Some(row),
                    Some(schema),
                )
                .await?
            } else {
                eval_expr(&order_expr.expr, Some(row), Some(schema))?
            };
            keys.push(value);
        }
        Ok(keys)
    }

    fn compare_keys(&self, keys_a: &[Value], keys_b: &[Value]) -> std::cmp::Ordering {
        for (i, order_expr) in self.order_by.iter().enumerate() {
            let asc = order_expr.asc.unwrap_or(true);
            let nulls_first = order_expr.nulls_first.unwrap_or(!asc);

            let ordering = compare_order_by_values(&keys_a[i], &keys_b[i], asc, nulls_first);
            if ordering != std::cmp::Ordering::Equal {
                return ordering;
            }
        }
        std::cmp::Ordering::Equal
    }
}

#[async_trait]
impl PhysicalOperator for SortOperator {
    fn schema(&self) -> &TableSchema {
        self.child.schema()
    }

    async fn open(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.child.open(ctx).await?;

        let max_sort_bytes = crate::session_context::current_max_sort_bytes();
        let mut total_bytes = 0usize;
        let mut rows = Vec::new();
        while let Some(row) = self.child.next(ctx).await? {
            enforce_sort_memory_limit(&mut total_bytes, &row, max_sort_bytes)?;
            rows.push(row);
        }
        let mut keyed_rows: Vec<(Vec<Value>, Row)> = Vec::with_capacity(rows.len());
        for row in rows {
            let keys = self.compute_sort_keys(&row, ctx).await?;
            keyed_rows.push((keys, row));
        }
        keyed_rows.sort_by(|(keys_a, _), (keys_b, _)| self.compare_keys(keys_a, keys_b));
        let rows = keyed_rows.into_iter().map(|(_, row)| row).collect();

        self.sorted_rows = rows;
        self.position = 0;
        self.opened = true;
        Ok(())
    }

    async fn next(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>> {
        if !self.opened {
            return Err(anyhow!("Operator not opened"));
        }

        if self.position < self.sorted_rows.len() {
            let row = self.sorted_rows[self.position].clone();
            self.position += 1;
            Ok(Some(row))
        } else {
            Ok(None)
        }
    }

    async fn close(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.child.close(ctx).await?;
        self.sorted_rows.clear();
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
        "Sort"
    }

    fn explain_info(&self) -> Option<String> {
        let keys: Vec<String> = self
            .order_by
            .iter()
            .map(|o| {
                let dir = if o.asc.unwrap_or(true) { "ASC" } else { "DESC" };
                format!("{} {}", o.expr, dir)
            })
            .collect();
        Some(format!("order_by=[{}]", keys.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ColumnDef, DataType};
    use sqlparser::ast::Ident;

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
                    name: "name".to_string(),
                    data_type: DataType::Text,
                    nullable: true,
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
    fn test_sort_creation() {
        use super::super::scan::TableScanOperator;
        use sqlparser::ast::Expr;

        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let order_by = vec![OrderByExpr {
            expr: Expr::Identifier(Ident::new("id")),
            asc: Some(true),
            nulls_first: None,
        }];

        let sort = SortOperator::new(child, order_by);

        assert_eq!(sort.name(), "Sort");
        assert!(!sort.opened);
    }

    #[test]
    fn test_sort_explain_info() {
        use super::super::scan::TableScanOperator;
        use sqlparser::ast::Expr;

        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let order_by = vec![
            OrderByExpr {
                expr: Expr::Identifier(Ident::new("id")),
                asc: Some(true),
                nulls_first: None,
            },
            OrderByExpr {
                expr: Expr::Identifier(Ident::new("name")),
                asc: Some(false),
                nulls_first: None,
            },
        ];

        let sort = SortOperator::new(child, order_by);

        let info = sort.explain_info().unwrap();
        assert!(info.contains("id ASC"));
        assert!(info.contains("name DESC"));
    }

    #[test]
    fn test_compare_rows() {
        use super::super::scan::TableScanOperator;
        use sqlparser::ast::Expr;

        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let order_by = vec![OrderByExpr {
            expr: Expr::Identifier(Ident::new("id")),
            asc: Some(true),
            nulls_first: None,
        }];

        let sort = SortOperator::new(child, order_by);

        let keys1 = vec![Value::Int32(1)];
        let keys2 = vec![Value::Int32(2)];

        assert_eq!(sort.compare_keys(&keys1, &keys2), std::cmp::Ordering::Less);
        assert_eq!(
            sort.compare_keys(&keys2, &keys1),
            std::cmp::Ordering::Greater
        );
        assert_eq!(sort.compare_keys(&keys1, &keys1), std::cmp::Ordering::Equal);
    }

    #[test]
    fn test_compare_keys_desc_ordering() {
        use super::super::scan::TableScanOperator;
        use sqlparser::ast::Expr;

        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let order_by = vec![OrderByExpr {
            expr: Expr::Identifier(Ident::new("id")),
            asc: Some(false),
            nulls_first: None,
        }];

        let sort = SortOperator::new(child, order_by);

        let keys1 = vec![Value::Int32(1)];
        let keys2 = vec![Value::Int32(2)];

        assert_eq!(
            sort.compare_keys(&keys1, &keys2),
            std::cmp::Ordering::Greater
        );
        assert_eq!(sort.compare_keys(&keys2, &keys1), std::cmp::Ordering::Less);
    }

    #[test]
    fn test_compare_keys_null_handling() {
        use super::super::scan::TableScanOperator;
        use sqlparser::ast::Expr;

        let schema = test_schema();

        // ASC + NULLS FIRST: NULL < non-NULL
        let child = Box::new(TableScanOperator::new(schema.clone()));
        let order_by = vec![OrderByExpr {
            expr: Expr::Identifier(Ident::new("id")),
            asc: Some(true),
            nulls_first: Some(true),
        }];
        let sort = SortOperator::new(child, order_by);

        let null_key = vec![Value::Null];
        let val_key = vec![Value::Int32(1)];

        assert_eq!(
            sort.compare_keys(&null_key, &val_key),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            sort.compare_keys(&val_key, &null_key),
            std::cmp::Ordering::Greater
        );

        // ASC + NULLS LAST: NULL > non-NULL
        let child = Box::new(TableScanOperator::new(schema));
        let order_by = vec![OrderByExpr {
            expr: Expr::Identifier(Ident::new("id")),
            asc: Some(true),
            nulls_first: Some(false),
        }];
        let sort = SortOperator::new(child, order_by);

        assert_eq!(
            sort.compare_keys(&null_key, &val_key),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            sort.compare_keys(&val_key, &null_key),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn test_compare_keys_multi_column_tiebreak() {
        use super::super::scan::TableScanOperator;
        use sqlparser::ast::Expr;

        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let order_by = vec![
            OrderByExpr {
                expr: Expr::Identifier(Ident::new("name")),
                asc: Some(true),
                nulls_first: None,
            },
            OrderByExpr {
                expr: Expr::Identifier(Ident::new("id")),
                asc: Some(false),
                nulls_first: None,
            },
        ];

        let sort = SortOperator::new(child, order_by);

        // Same first key, tiebreak by second key (DESC)
        let keys_a = vec![Value::Text("Alice".to_string()), Value::Int32(1)];
        let keys_b = vec![Value::Text("Alice".to_string()), Value::Int32(2)];

        // Second column is DESC, so 2 comes before 1
        assert_eq!(
            sort.compare_keys(&keys_a, &keys_b),
            std::cmp::Ordering::Greater
        );

        // Different first key — tiebreak not needed
        let keys_c = vec![Value::Text("Bob".to_string()), Value::Int32(1)];
        assert_eq!(
            sort.compare_keys(&keys_a, &keys_c),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn test_estimated_row_size_includes_payload() {
        let small = Row::new(vec![Value::Int32(1), Value::Text("a".to_string())]);
        let big = Row::new(vec![
            Value::Int32(1),
            Value::Text("a".repeat(1024)),
            Value::Json("{\"k\":\"v\"}".repeat(128)),
        ]);

        assert!(estimated_row_size(&big) > estimated_row_size(&small));
    }

    #[test]
    fn test_sort_memory_limit_within_limit() {
        let row = Row::new(vec![Value::Text("abc".to_string())]);
        let mut total_bytes = 0usize;
        let limit = estimated_row_size(&row) + 1;

        enforce_sort_memory_limit(&mut total_bytes, &row, limit).unwrap();
        assert!(total_bytes <= limit);
    }

    #[test]
    fn test_sort_memory_limit_exceeded() {
        let row = Row::new(vec![Value::Text("x".repeat(1024))]);
        let mut total_bytes = 0usize;
        let limit = estimated_row_size(&row) - 1;

        let err = enforce_sort_memory_limit(&mut total_bytes, &row, limit).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ORDER BY sort memory limit exceeded"));
        assert!(msg.contains("pgtikv.max_sort_bytes"));
    }

    #[test]
    fn test_sort_memory_limit_zero_is_unlimited() {
        let row = Row::new(vec![Value::Text("x".repeat(2048))]);
        let mut total_bytes = 0usize;

        enforce_sort_memory_limit(&mut total_bytes, &row, 0).unwrap();
        assert_eq!(total_bytes, 0);
    }
}
