use std::collections::HashSet;

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use super::key_encoding::encode_values_key;
use super::{BoxedOperator, ExecutionContext, PhysicalOperator};
use crate::model::{Row, TableSchema};
use crate::pool::try_grow_statement_memory_scope;
use crate::sql::analyzer::types::TypedExpr;
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::memory::estimate_key_size;

#[derive(Debug)]
pub struct DistinctOperator {
    child: BoxedOperator,
    seen: HashSet<Vec<u8>>,
    opened: bool,
}

impl DistinctOperator {
    pub fn new(child: BoxedOperator) -> Self {
        Self {
            child,
            seen: HashSet::new(),
            opened: false,
        }
    }

    fn row_to_key(row: &Row) -> Vec<u8> {
        encode_values_key(&row.values)
    }
}

#[async_trait]
impl PhysicalOperator for DistinctOperator {
    fn schema(&self) -> &TableSchema {
        self.child.schema()
    }

    async fn open(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.child.open(ctx).await?;
        self.seen.clear();
        self.opened = true;
        Ok(())
    }

    async fn next(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>> {
        if !self.opened {
            return Err(anyhow!("Operator not opened"));
        }

        while let Some(row) = self.child.next(ctx).await? {
            let key = Self::row_to_key(&row);
            let key_bytes = estimate_key_size(key.as_slice());
            if self.seen.insert(key) {
                try_grow_statement_memory_scope("operators.distinct.hashset", key_bytes)?;
                return Ok(Some(row));
            }
        }

        Ok(None)
    }

    async fn close(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.child.close(ctx).await?;
        self.seen.clear();
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
        "Distinct"
    }

    fn explain_info(&self) -> Option<String> {
        None
    }
}

#[derive(Debug)]
pub struct DistinctOnOperator {
    child: BoxedOperator,
    on_exprs: Vec<TypedExpr>,
    seen: HashSet<Vec<u8>>,
    opened: bool,
}

impl DistinctOnOperator {
    pub fn new(child: BoxedOperator, on_exprs: Vec<TypedExpr>) -> Self {
        Self {
            child,
            on_exprs,
            seen: HashSet::new(),
            opened: false,
        }
    }

    fn compute_key(
        &self,
        row: &Row,
        query_ctx: &crate::sql::query_context::QueryContext,
    ) -> Result<Vec<u8>> {
        let mut key_values = Vec::new();
        for expr in &self.on_exprs {
            key_values.push(eval_typed_expr(expr, row, query_ctx)?);
        }
        Ok(encode_values_key(&key_values))
    }
}

#[async_trait]
impl PhysicalOperator for DistinctOnOperator {
    fn schema(&self) -> &TableSchema {
        self.child.schema()
    }

    async fn open(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.child.open(ctx).await?;
        self.seen.clear();
        self.opened = true;
        Ok(())
    }

    async fn next(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>> {
        if !self.opened {
            return Err(anyhow!("Operator not opened"));
        }

        while let Some(row) = self.child.next(ctx).await? {
            let key = self.compute_key(&row, ctx.query_ctx)?;
            let key_bytes = estimate_key_size(key.as_slice());
            if self.seen.insert(key) {
                try_grow_statement_memory_scope("operators.distinct_on.hashset", key_bytes)?;
                return Ok(Some(row));
            }
        }

        Ok(None)
    }

    async fn close(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.child.close(ctx).await?;
        self.seen.clear();
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
        "DistinctOn"
    }

    fn explain_info(&self) -> Option<String> {
        let exprs: Vec<String> = self.on_exprs.iter().map(|e| format!("{:?}", e)).collect();
        Some(format!("on=[{}]", exprs.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ColumnDef, DataType, Value};
    use crate::sql::analyzer::types::TypedExprKind;
    use crate::sql::operators::scan::TableScanOperator;
    use crate::sql::query_context::QueryContext;

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
                    generation_expr: None,
                    generation_expr_authorized_by: None,
                    collation: None,
                    is_dropped: false,
                },
                ColumnDef {
                    name: "name".to_string(),
                    data_type: DataType::Text,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    generation_expr: None,
                    generation_expr_authorized_by: None,
                    collation: None,
                    is_dropped: false,
                },
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![0],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            rls_enabled: false,
            rls_force: false,
            from_alias: None,
        }
    }

    #[test]
    fn test_distinct_operator_creation() {
        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));
        let op = DistinctOperator::new(child);

        assert_eq!(op.name(), "Distinct");
        assert!(!op.opened);
    }

    #[test]
    fn test_distinct_on_operator_creation() {
        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));
        let on_exprs = vec![TypedExpr {
            kind: TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: 1,
                column_name: "name".to_string(),
            },
            data_type: DataType::Text,
        }];
        let op = DistinctOnOperator::new(child, on_exprs);

        assert_eq!(op.name(), "DistinctOn");
        assert!(op.explain_info().unwrap().contains("name"));
    }

    #[test]
    fn test_row_to_key_canonicalizes_numeric_scales() {
        use rust_decimal::Decimal;
        use std::str::FromStr;

        let d1 = Decimal::from_str("1.0").unwrap();
        let d2 = Decimal::from_str("1.00").unwrap();

        let row1 = Row::new(vec![Value::Numeric(d1)]);
        let row2 = Row::new(vec![Value::Numeric(d2)]);
        assert_eq!(
            DistinctOperator::row_to_key(&row1),
            DistinctOperator::row_to_key(&row2)
        );
    }

    #[test]
    fn test_distinct_operator_metadata_helpers() {
        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema.clone()));
        let mut op = DistinctOperator::new(child);

        assert_eq!(op.schema().name, schema.name);
        assert_eq!(op.explain_info(), None);
        let children = op.children();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].schema().name, "test");

        let children_mut = op.children_mut();
        assert_eq!(children_mut.len(), 1);
        assert_eq!(children_mut[0].schema().name, "test");
    }

    #[test]
    fn test_distinct_on_compute_key_uses_expr_projection() {
        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));
        let on_exprs = vec![TypedExpr {
            kind: TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: 1,
                column_name: "name".to_string(),
            },
            data_type: DataType::Text,
        }];
        let op = DistinctOnOperator::new(child, on_exprs);
        let query_ctx = QueryContext::from_task_locals();

        let row_a = Row::new(vec![Value::Int32(1), Value::Text("alice".to_string())]);
        let row_b = Row::new(vec![Value::Int32(2), Value::Text("alice".to_string())]);
        let row_c = Row::new(vec![Value::Int32(3), Value::Text("bob".to_string())]);

        let key_a = op.compute_key(&row_a, &query_ctx).unwrap();
        let key_b = op.compute_key(&row_b, &query_ctx).unwrap();
        let key_c = op.compute_key(&row_c, &query_ctx).unwrap();

        assert_eq!(key_a, key_b, "same DISTINCT ON value should share key");
        assert_ne!(key_a, key_c, "different DISTINCT ON value must differ");
    }
}
