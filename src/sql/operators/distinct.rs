use std::collections::HashSet;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use sqlparser::ast::Expr;

use super::{BoxedOperator, ExecutionContext, PhysicalOperator};
use crate::sql::expr::eval_expr;
use crate::sql::value_key::serialize_values_for_key;
use crate::types::{Row, TableSchema};

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
        serialize_values_for_key(&row.values).unwrap_or_default()
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
            if self.seen.insert(key) {
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
    on_exprs: Vec<Expr>,
    seen: HashSet<Vec<u8>>,
    opened: bool,
}

impl DistinctOnOperator {
    pub fn new(child: BoxedOperator, on_exprs: Vec<Expr>) -> Self {
        Self {
            child,
            on_exprs,
            seen: HashSet::new(),
            opened: false,
        }
    }

    fn compute_key(&self, row: &Row, schema: &TableSchema) -> Result<Vec<u8>> {
        let mut key_values = Vec::new();
        for expr in &self.on_exprs {
            key_values.push(eval_expr(expr, Some(row), Some(schema))?);
        }
        Ok(serialize_values_for_key(&key_values).unwrap_or_default())
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

        let schema = self.child.schema().clone();
        while let Some(row) = self.child.next(ctx).await? {
            let key = self.compute_key(&row, &schema)?;
            if self.seen.insert(key) {
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
        let exprs: Vec<String> = self.on_exprs.iter().map(|e| format!("{}", e)).collect();
        Some(format!("on=[{}]", exprs.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::operators::scan::TableScanOperator;
    use crate::types::{ColumnDef, DataType, Value};

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
        use sqlparser::ast::Ident;

        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));
        let on_exprs = vec![Expr::Identifier(Ident::new("name"))];
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
}
