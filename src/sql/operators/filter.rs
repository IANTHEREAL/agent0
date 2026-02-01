use anyhow::{anyhow, Result};
use async_trait::async_trait;
use sqlparser::ast::Expr;

use super::{BoxedOperator, ExecutionContext, PhysicalOperator};
use crate::sql::expr::{coerce_text_literal_to_bool, eval_expr, validate_bool_expr_in_boolean_context};
use crate::types::{Row, TableSchema, Value};

#[derive(Debug)]
pub struct FilterOperator {
    child: BoxedOperator,
    predicate: Expr,
    opened: bool,
}

impl FilterOperator {
    pub fn new(child: BoxedOperator, predicate: Expr) -> Self {
        Self {
            child,
            predicate,
            opened: false,
        }
    }

    fn evaluate_predicate(&self, row: &Row) -> Result<bool> {
        let result = eval_expr(&self.predicate, Some(row), Some(self.child.schema()))?;
        let result = coerce_text_literal_to_bool(&self.predicate, result)?;
        match result {
            Value::Boolean(b) => Ok(b),
            Value::Null => Ok(false),
            _ => Err(anyhow!("Filter predicate must evaluate to boolean")),
        }
    }
}

#[async_trait]
impl PhysicalOperator for FilterOperator {
    fn schema(&self) -> &TableSchema {
        self.child.schema()
    }

    async fn open(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        validate_bool_expr_in_boolean_context(
            &self.predicate,
            self.child.schema(),
            "Filter predicate must evaluate to boolean",
        )?;
        self.child.open(ctx).await?;
        self.opened = true;
        Ok(())
    }

    async fn next(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>> {
        if !self.opened {
            return Err(anyhow!("Operator not opened"));
        }

        while let Some(row) = self.child.next(ctx).await? {
            if self.evaluate_predicate(&row)? {
                return Ok(Some(row));
            }
        }

        Ok(None)
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
        "Filter"
    }

    fn explain_info(&self) -> Option<String> {
        Some(format!("predicate={}", self.predicate))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ColumnDef, DataType};
    use sqlparser::ast::{BinaryOperator, Ident};

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
                    name: "active".to_string(),
                    data_type: DataType::Boolean,
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
    fn test_filter_creation() {
        use super::super::scan::TableScanOperator;

        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));
        
        let predicate = Expr::BinaryOp {
            left: Box::new(Expr::Identifier(Ident::new("id"))),
            op: BinaryOperator::Gt,
            right: Box::new(Expr::Value(sqlparser::ast::Value::Number("5".to_string(), false))),
        };

        let filter = FilterOperator::new(child, predicate.clone());

        assert_eq!(filter.name(), "Filter");
        assert!(!filter.opened);
        assert!(filter.explain_info().unwrap().contains("id > 5"));
    }

    #[test]
    fn test_predicate_evaluation() {
        use super::super::scan::TableScanOperator;

        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema.clone()));
        
        let predicate = Expr::Identifier(Ident::new("active"));
        let filter = FilterOperator::new(child, predicate);

        let row_true = Row::new(vec![Value::Int32(1), Value::Boolean(true)]);
        let row_false = Row::new(vec![Value::Int32(2), Value::Boolean(false)]);

        assert!(filter.evaluate_predicate(&row_true).unwrap());
        assert!(!filter.evaluate_predicate(&row_false).unwrap());
    }

    #[test]
    fn test_predicate_evaluation_text_boolean_literals() {
        use super::super::scan::TableScanOperator;

        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema.clone()));

        let predicate =
            Expr::Value(sqlparser::ast::Value::SingleQuotedString("true".to_string()));
        let filter = FilterOperator::new(child, predicate);

        let row = Row::new(vec![Value::Int32(1), Value::Boolean(false)]);
        assert!(filter.evaluate_predicate(&row).unwrap());

        let child = Box::new(TableScanOperator::new(schema));
        let predicate =
            Expr::Value(sqlparser::ast::Value::SingleQuotedString("false".to_string()));
        let filter = FilterOperator::new(child, predicate);
        assert!(!filter.evaluate_predicate(&row).unwrap());
    }
}
