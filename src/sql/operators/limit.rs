use anyhow::{anyhow, Result};
use async_trait::async_trait;

use super::{BoxedOperator, ExecutionContext, PhysicalOperator};
use crate::model::{Row, TableSchema, Value};
use crate::sql::analyzer::types::TypedExpr;
use crate::sql::expr::typed_eval::{eval_const_usize, eval_typed_expr};
use crate::sql::query_context::QueryContext;

#[derive(Debug)]
pub struct LimitOperator {
    child: BoxedOperator,
    limit_expr: Option<TypedExpr>,
    offset_expr: Option<TypedExpr>,
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
            limit_expr: None,
            offset_expr: None,
            limit,
            offset,
            rows_returned: 0,
            rows_skipped: 0,
            opened: false,
        }
    }

    pub fn new_with_exprs(
        child: BoxedOperator,
        limit_expr: Option<TypedExpr>,
        offset_expr: Option<TypedExpr>,
    ) -> Self {
        Self {
            child,
            limit_expr,
            offset_expr,
            limit: None,
            offset: 0,
            rows_returned: 0,
            rows_skipped: 0,
            opened: false,
        }
    }

    fn constant_limit(&self) -> Option<usize> {
        self.limit_expr
            .as_ref()
            .and_then(|expr| eval_const_usize(expr, false).ok())
            .or(self.limit)
    }

    #[allow(dead_code)] // framework: supports explain_info trait method
    fn constant_offset(&self) -> Option<usize> {
        self.offset_expr
            .as_ref()
            .and_then(|expr| eval_const_usize(expr, false).ok())
            .or(if self.offset > 0 {
                Some(self.offset)
            } else {
                None
            })
    }
}

#[async_trait]
impl PhysicalOperator for LimitOperator {
    fn schema(&self) -> &TableSchema {
        self.child.schema()
    }

    async fn open(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.child.open(ctx).await?;
        if let Some(expr) = &self.limit_expr {
            self.limit = Some(evaluate_limit_bound(expr, ctx.query_ctx, "LIMIT")?);
        }
        if let Some(expr) = &self.offset_expr {
            self.offset = evaluate_limit_bound(expr, ctx.query_ctx, "OFFSET")?;
        }
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
        let limit = self.constant_limit();
        let offset = self.constant_offset().unwrap_or(0);
        match (limit, offset) {
            (Some(l), 0) => Some(format!("limit={}", l)),
            (Some(l), o) => Some(format!("limit={}, offset={}", l, o)),
            (None, o) if o > 0 => Some(format!("offset={}", o)),
            _ => None,
        }
    }

    fn estimated_rows(&self) -> Option<usize> {
        self.constant_limit()
    }
}

fn evaluate_limit_bound(expr: &TypedExpr, qctx: &QueryContext, clause: &str) -> Result<usize> {
    let value = eval_typed_expr(expr, &Row::new(vec![]), qctx)?;
    let n = match value {
        Value::Int32(v) => i64::from(v),
        Value::Int64(v) => v,
        Value::Null => return Err(anyhow!("{clause} must not be NULL")),
        other => {
            return Err(anyhow!(
                "{clause} must evaluate to a non-negative integer, got: {:?}",
                other
            ))
        }
    };
    if n < 0 {
        return Err(anyhow!("{clause} must not be negative"));
    }
    usize::try_from(n).map_err(|_| anyhow!("{clause} is too large"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ColumnDef, DataType};
    use crate::sql::analyzer::types::{TypedExpr, TypedExprKind};
    use crate::sql::query_context::QueryContext;

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
                collation: None,
            }],
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
        assert_eq!(
            limit2.explain_info(),
            Some("limit=10, offset=5".to_string())
        );

        let child3 = Box::new(TableScanOperator::new(schema.clone()));
        let limit3 = LimitOperator::new(child3, None, 5);
        assert_eq!(limit3.explain_info(), Some("offset=5".to_string()));

        let child4 = Box::new(TableScanOperator::new(schema));
        let limit4 = LimitOperator::new(child4, None, 0);
        assert_eq!(limit4.explain_info(), None);
    }

    #[test]
    fn test_evaluate_limit_bound_parameter() {
        let mut qctx = QueryContext::for_tests();
        qctx.params = vec![Some(Value::Int64(10)), Some(Value::Int64(5))];
        let limit_expr = TypedExpr::new(TypedExprKind::Parameter { index: 0 }, DataType::Int64);
        let offset_expr = TypedExpr::new(TypedExprKind::Parameter { index: 1 }, DataType::Int64);
        assert_eq!(
            evaluate_limit_bound(&limit_expr, &qctx, "LIMIT").unwrap(),
            10
        );
        assert_eq!(
            evaluate_limit_bound(&offset_expr, &qctx, "OFFSET").unwrap(),
            5
        );
    }

    #[test]
    fn test_evaluate_limit_bound_negative_rejected() {
        let mut qctx = QueryContext::for_tests();
        qctx.params = vec![Some(Value::Int64(-1))];
        let limit_expr = TypedExpr::new(TypedExprKind::Parameter { index: 0 }, DataType::Int64);
        let err = evaluate_limit_bound(&limit_expr, &qctx, "LIMIT").unwrap_err();
        assert!(err.to_string().contains("must not be negative"));
    }
}
