use anyhow::{anyhow, Result};
use async_trait::async_trait;

use super::{BoxedOperator, ExecutionContext, PhysicalOperator};
use crate::sql::analyzer::types::TypedExpr;
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::query_context::QueryContext;
use crate::types::{Row, TableSchema, Value};

#[derive(Debug)]
pub struct FilterOperator {
    child: BoxedOperator,
    predicate: TypedExpr,
    opened: bool,
}

impl FilterOperator {
    pub fn new(child: BoxedOperator, predicate: TypedExpr) -> Self {
        Self {
            child,
            predicate,
            opened: false,
        }
    }

    fn evaluate_predicate(&self, row: &Row, query_ctx: &QueryContext) -> Result<bool> {
        let result = eval_typed_expr(&self.predicate, row, query_ctx)?;
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
        self.child.open(ctx).await?;
        self.opened = true;
        Ok(())
    }

    async fn next(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>> {
        if !self.opened {
            return Err(anyhow!("Operator not opened"));
        }

        while let Some(row) = self.child.next(ctx).await? {
            if self.evaluate_predicate(&row, ctx.query_ctx)? {
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
        Some(format!("predicate={:?}", self.predicate))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::analyzer::types::{BinaryOp, IsTestKind, TypedExprKind};
    use crate::types::{ColumnDef, DataType};

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
            from_alias: None,
        }
    }

    #[test]
    fn test_filter_creation() {
        use super::super::scan::TableScanOperator;

        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let left_typed = TypedExpr {
            kind: TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: 0,
                column_name: "id".to_string(),
            },
            data_type: DataType::Int32,
        };
        let right_typed = TypedExpr {
            kind: TypedExprKind::Constant(Value::Int64(5)),
            data_type: DataType::Int64,
        };
        let predicate = TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(left_typed),
                op: BinaryOp::Gt,
                right: Box::new(right_typed),
            },
            data_type: DataType::Boolean,
        };

        let filter = FilterOperator::new(child, predicate);

        assert_eq!(filter.name(), "Filter");
        assert!(!filter.opened);
        let info = filter.explain_info().unwrap();
        assert!(info.contains("id"));
        assert!(info.contains("Gt"));
    }

    #[test]
    fn test_predicate_evaluation() {
        use super::super::scan::TableScanOperator;

        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema.clone()));

        let predicate = TypedExpr {
            kind: TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: 1,
                column_name: "active".to_string(),
            },
            data_type: DataType::Boolean,
        };
        let filter = FilterOperator::new(child, predicate);

        let row_true = Row::new(vec![Value::Int32(1), Value::Boolean(true)]);
        let row_false = Row::new(vec![Value::Int32(2), Value::Boolean(false)]);

        assert!(filter
            .evaluate_predicate(&row_true, &QueryContext::from_task_locals())
            .unwrap());
        assert!(!filter
            .evaluate_predicate(&row_false, &QueryContext::from_task_locals())
            .unwrap());
    }

    #[test]
    fn test_predicate_evaluation_boolean_literals() {
        use super::super::scan::TableScanOperator;

        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema.clone()));

        let predicate = TypedExpr {
            kind: TypedExprKind::Constant(Value::Boolean(true)),
            data_type: DataType::Boolean,
        };
        let filter = FilterOperator::new(child, predicate);

        let row = Row::new(vec![Value::Int32(1), Value::Boolean(false)]);
        assert!(filter
            .evaluate_predicate(&row, &QueryContext::from_task_locals())
            .unwrap());

        let child = Box::new(TableScanOperator::new(schema));
        let predicate = TypedExpr {
            kind: TypedExprKind::Constant(Value::Boolean(false)),
            data_type: DataType::Boolean,
        };
        let filter = FilterOperator::new(child, predicate);
        assert!(!filter
            .evaluate_predicate(&row, &QueryContext::from_task_locals())
            .unwrap());
    }

    #[test]
    fn test_filter_predicate_null_returns_false() {
        use super::super::scan::TableScanOperator;

        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        // Predicate: active (column is NULL -> should return false)
        let predicate = TypedExpr {
            kind: TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: 1,
                column_name: "active".to_string(),
            },
            data_type: DataType::Boolean,
        };
        let filter = FilterOperator::new(child, predicate);

        let row_null = Row::new(vec![Value::Int32(1), Value::Null]);
        assert!(!filter
            .evaluate_predicate(&row_null, &QueryContext::from_task_locals())
            .unwrap());
    }

    #[test]
    fn test_filter_predicate_comparison_operators() {
        use super::super::scan::TableScanOperator;

        let schema = test_schema();

        // Predicate: id > 2
        let child = Box::new(TableScanOperator::new(schema.clone()));
        let predicate = TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(TypedExpr {
                    kind: TypedExprKind::ColumnRef {
                        scope_depth: 0,
                        column_index: 0,
                        column_name: "id".to_string(),
                    },
                    data_type: DataType::Int32,
                }),
                op: BinaryOp::Gt,
                right: Box::new(TypedExpr {
                    kind: TypedExprKind::Constant(Value::Int64(2)),
                    data_type: DataType::Int64,
                }),
            },
            data_type: DataType::Boolean,
        };
        let filter = FilterOperator::new(child, predicate);

        let row1 = Row::new(vec![Value::Int32(1), Value::Boolean(true)]);
        let row2 = Row::new(vec![Value::Int32(2), Value::Boolean(true)]);
        let row3 = Row::new(vec![Value::Int32(3), Value::Boolean(true)]);

        assert!(!filter
            .evaluate_predicate(&row1, &QueryContext::from_task_locals())
            .unwrap());
        assert!(!filter
            .evaluate_predicate(&row2, &QueryContext::from_task_locals())
            .unwrap());
        assert!(filter
            .evaluate_predicate(&row3, &QueryContext::from_task_locals())
            .unwrap());

        // Predicate: id = 2
        let child = Box::new(TableScanOperator::new(schema));
        let predicate = TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(TypedExpr {
                    kind: TypedExprKind::ColumnRef {
                        scope_depth: 0,
                        column_index: 0,
                        column_name: "id".to_string(),
                    },
                    data_type: DataType::Int32,
                }),
                op: BinaryOp::Eq,
                right: Box::new(TypedExpr {
                    kind: TypedExprKind::Constant(Value::Int64(2)),
                    data_type: DataType::Int64,
                }),
            },
            data_type: DataType::Boolean,
        };
        let filter = FilterOperator::new(child, predicate);

        assert!(!filter
            .evaluate_predicate(&row1, &QueryContext::from_task_locals())
            .unwrap());
        assert!(filter
            .evaluate_predicate(&row2, &QueryContext::from_task_locals())
            .unwrap());
        assert!(!filter
            .evaluate_predicate(&row3, &QueryContext::from_task_locals())
            .unwrap());
    }

    #[test]
    fn test_filter_predicate_is_null() {
        use super::super::scan::TableScanOperator;

        let schema = test_schema();

        // Predicate: active IS NULL
        let child = Box::new(TableScanOperator::new(schema.clone()));
        let predicate = TypedExpr {
            kind: TypedExprKind::IsTest {
                expr: Box::new(TypedExpr {
                    kind: TypedExprKind::ColumnRef {
                        scope_depth: 0,
                        column_index: 1,
                        column_name: "active".to_string(),
                    },
                    data_type: DataType::Boolean,
                }),
                test: IsTestKind::Null,
                negated: false,
            },
            data_type: DataType::Boolean,
        };
        let filter = FilterOperator::new(child, predicate);

        let row_null = Row::new(vec![Value::Int32(1), Value::Null]);
        let row_true = Row::new(vec![Value::Int32(2), Value::Boolean(true)]);

        assert!(filter
            .evaluate_predicate(&row_null, &QueryContext::from_task_locals())
            .unwrap());
        assert!(!filter
            .evaluate_predicate(&row_true, &QueryContext::from_task_locals())
            .unwrap());

        // Predicate: active IS NOT NULL
        let child = Box::new(TableScanOperator::new(schema));
        let predicate = TypedExpr {
            kind: TypedExprKind::IsTest {
                expr: Box::new(TypedExpr {
                    kind: TypedExprKind::ColumnRef {
                        scope_depth: 0,
                        column_index: 1,
                        column_name: "active".to_string(),
                    },
                    data_type: DataType::Boolean,
                }),
                test: IsTestKind::Null,
                negated: true,
            },
            data_type: DataType::Boolean,
        };
        let filter = FilterOperator::new(child, predicate);

        assert!(!filter
            .evaluate_predicate(&row_null, &QueryContext::from_task_locals())
            .unwrap());
        assert!(filter
            .evaluate_predicate(&row_true, &QueryContext::from_task_locals())
            .unwrap());
    }
}
