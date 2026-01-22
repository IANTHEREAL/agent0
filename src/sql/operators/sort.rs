use anyhow::{anyhow, Result};
use async_trait::async_trait;
use sqlparser::ast::OrderByExpr;

use super::{collect_all, BoxedOperator, ExecutionContext, PhysicalOperator};
use crate::sql::expr::{compare_order_by_values, eval_expr};
use crate::types::{Row, TableSchema, Value};

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

    fn compute_sort_keys(&self, row: &Row) -> Result<Vec<Value>> {
        let schema = self.child.schema();
        let mut keys = Vec::with_capacity(self.order_by.len());
        for order_expr in &self.order_by {
            keys.push(eval_expr(&order_expr.expr, Some(row), Some(schema))?);
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

        let rows = collect_all(self.child.as_mut(), ctx).await?;
        let mut keyed_rows: Vec<(Vec<Value>, Row)> = Vec::with_capacity(rows.len());
        for row in rows {
            let keys = self.compute_sort_keys(&row)?;
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

        let row1 = Row::new(vec![Value::Int32(1), Value::Text("Alice".to_string())]);
        let row2 = Row::new(vec![Value::Int32(2), Value::Text("Bob".to_string())]);

        let keys1 = sort.compute_sort_keys(&row1).unwrap();
        let keys2 = sort.compute_sort_keys(&row2).unwrap();

        assert_eq!(
            sort.compare_keys(&keys1, &keys2),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            sort.compare_keys(&keys2, &keys1),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            sort.compare_keys(&keys1, &keys1),
            std::cmp::Ordering::Equal
        );
    }
}
