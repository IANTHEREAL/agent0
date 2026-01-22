use anyhow::{anyhow, Result};
use async_trait::async_trait;

use super::{BoxedOperator, ExecutionContext, PhysicalOperator};
use crate::types::{Row, TableSchema};

#[derive(Debug)]
pub struct LimitOperator {
    child: BoxedOperator,
    limit: Option<usize>,
    offset: usize,
    rows_returned: usize,
    rows_skipped: usize,
    opened: bool,
}

impl LimitOperator {
    pub fn new(child: BoxedOperator, limit: Option<usize>, offset: usize) -> Self {
        Self {
            child,
            limit,
            offset,
            rows_returned: 0,
            rows_skipped: 0,
            opened: false,
        }
    }
}

#[async_trait]
impl PhysicalOperator for LimitOperator {
    fn schema(&self) -> &TableSchema {
        self.child.schema()
    }

    async fn open(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.child.open(ctx).await?;
        self.rows_returned = 0;
        self.rows_skipped = 0;
        self.opened = true;
        Ok(())
    }

    async fn next(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>> {
        if !self.opened {
            return Err(anyhow!("Operator not opened"));
        }

        if let Some(limit) = self.limit {
            if self.rows_returned >= limit {
                return Ok(None);
            }
        }

        while self.rows_skipped < self.offset {
            if self.child.next(ctx).await?.is_none() {
                return Ok(None);
            }
            self.rows_skipped += 1;
        }

        if let Some(row) = self.child.next(ctx).await? {
            self.rows_returned += 1;
            Ok(Some(row))
        } else {
            Ok(None)
        }
    }

    async fn close(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.child.close(ctx).await?;
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
        "Limit"
    }

    fn explain_info(&self) -> Option<String> {
        match (self.limit, self.offset) {
            (Some(l), 0) => Some(format!("limit={}", l)),
            (Some(l), o) => Some(format!("limit={}, offset={}", l, o)),
            (None, o) if o > 0 => Some(format!("offset={}", o)),
            _ => None,
        }
    }

    fn estimated_rows(&self) -> Option<usize> {
        self.limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ColumnDef, DataType};

    fn test_schema() -> TableSchema {
        TableSchema {
            name: "test".to_string(),
            table_id: 1,
            columns: vec![ColumnDef {
                name: "id".to_string(),
                data_type: DataType::Int32,
                nullable: false,
                primary_key: true,
                unique: false,
                is_serial: false,
                default_expr: None,
            }],
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
    fn test_limit_creation() {
        use super::super::scan::TableScanOperator;

        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));
        let limit = LimitOperator::new(child, Some(10), 5);

        assert_eq!(limit.name(), "Limit");
        assert_eq!(limit.estimated_rows(), Some(10));
    }

    #[test]
    fn test_limit_explain_info() {
        use super::super::scan::TableScanOperator;

        let schema = test_schema();

        let child1 = Box::new(TableScanOperator::new(schema.clone()));
        let limit1 = LimitOperator::new(child1, Some(10), 0);
        assert_eq!(limit1.explain_info(), Some("limit=10".to_string()));

        let child2 = Box::new(TableScanOperator::new(schema.clone()));
        let limit2 = LimitOperator::new(child2, Some(10), 5);
        assert_eq!(limit2.explain_info(), Some("limit=10, offset=5".to_string()));

        let child3 = Box::new(TableScanOperator::new(schema.clone()));
        let limit3 = LimitOperator::new(child3, None, 5);
        assert_eq!(limit3.explain_info(), Some("offset=5".to_string()));

        let child4 = Box::new(TableScanOperator::new(schema));
        let limit4 = LimitOperator::new(child4, None, 0);
        assert_eq!(limit4.explain_info(), None);
    }
}
