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
            // LIMIT NULL → no limit (PG 17 parity: LIMIT NULL ≡ LIMIT ALL)
            self.limit = evaluate_limit_bound(expr, ctx.query_ctx, "LIMIT")?;
        }
        if let Some(expr) = &self.offset_expr {
            // OFFSET NULL → offset 0 (PG 17 parity)
            self.offset = evaluate_limit_bound(expr, ctx.query_ctx, "OFFSET")?.unwrap_or(0);
        }
        // Clamp LIMIT to the DML row cap when set.
        // This prevents excessive memory consumption when a parameter-bound
        // LIMIT (e.g. LIMIT $1) exceeds the configured maximum for DML
        // auxiliary subqueries.  The cap is active within all DML execution
        // scopes: INSERT...SELECT, UPDATE FROM/SET subqueries,
        // DELETE USING/WHERE subqueries, and VALUES expression subqueries.
        if let Some(ref mut limit) = self.limit {
            let cap = crate::session_context::current_dml_limit_cap();
            if cap > 0 && *limit > cap {
                *limit = cap;
            }
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

fn evaluate_limit_bound(
    expr: &TypedExpr,
    qctx: &QueryContext,
    clause: &str,
) -> Result<Option<usize>> {
    let value = eval_typed_expr(expr, &Row::new(vec![]), qctx)?;
    let n = match value {
        Value::Int32(v) => i64::from(v),
        Value::Int64(v) => v,
        // PG 17 parity: NULL means "no bound" (LIMIT NULL ≡ LIMIT ALL, OFFSET NULL ≡ OFFSET 0)
        Value::Null => return Ok(None),
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
    usize::try_from(n)
        .map(Some)
        .map_err(|_| anyhow!("{clause} is too large"))
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
                generation_expr: None,
                generation_expr_authorized_by: None,
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
            Some(10)
        );
        assert_eq!(
            evaluate_limit_bound(&offset_expr, &qctx, "OFFSET").unwrap(),
            Some(5)
        );
    }

    #[test]
    fn test_evaluate_limit_bound_null_returns_none() {
        let mut qctx = QueryContext::for_tests();
        qctx.params = vec![Some(Value::Null)];
        let expr = TypedExpr::new(TypedExprKind::Parameter { index: 0 }, DataType::Int64);
        assert_eq!(evaluate_limit_bound(&expr, &qctx, "LIMIT").unwrap(), None);
        assert_eq!(evaluate_limit_bound(&expr, &qctx, "OFFSET").unwrap(), None);
    }

    #[test]
    fn test_evaluate_limit_bound_negative_rejected() {
        let mut qctx = QueryContext::for_tests();
        qctx.params = vec![Some(Value::Int64(-1))];
        let limit_expr = TypedExpr::new(TypedExprKind::Parameter { index: 0 }, DataType::Int64);
        let err = evaluate_limit_bound(&limit_expr, &qctx, "LIMIT").unwrap_err();
        assert!(err.to_string().contains("must not be negative"));
    }

    #[tokio::test]
    async fn test_dml_limit_cap_clamps_dynamic_limit() {
        use crate::session_context::{current_dml_limit_cap, with_dml_limit_cap};

        // Outside DML scope, cap is 0 (unlimited).
        assert_eq!(current_dml_limit_cap(), 0);

        // Inside a DML scope with cap=100, a dynamic LIMIT of 500 should be
        // clamped to 100.
        with_dml_limit_cap(100, async {
            assert_eq!(current_dml_limit_cap(), 100);

            // Simulate what LimitOperator::open() does after evaluating the
            // bound LIMIT expression.
            let mut limit: Option<usize> = Some(500);
            if let Some(ref mut l) = limit {
                let cap = current_dml_limit_cap();
                if cap > 0 && *l > cap {
                    *l = cap;
                }
            }
            assert_eq!(limit, Some(100));
        })
        .await;
    }

    #[tokio::test]
    async fn test_dml_limit_cap_no_clamp_when_under() {
        use crate::session_context::{current_dml_limit_cap, with_dml_limit_cap};

        with_dml_limit_cap(100, async {
            // LIMIT already under cap — should remain unchanged.
            let mut limit: Option<usize> = Some(50);
            if let Some(ref mut l) = limit {
                let cap = current_dml_limit_cap();
                if cap > 0 && *l > cap {
                    *l = cap;
                }
            }
            assert_eq!(limit, Some(50));
        })
        .await;
    }

    #[tokio::test]
    async fn test_dml_limit_cap_no_clamp_when_unset() {
        use crate::session_context::current_dml_limit_cap;

        // Without DML scope, cap is 0 → no clamping.
        let mut limit: Option<usize> = Some(99999);
        if let Some(ref mut l) = limit {
            let cap = current_dml_limit_cap();
            if cap > 0 && *l > cap {
                *l = cap;
            }
        }
        assert_eq!(limit, Some(99999));
    }

    #[tokio::test]
    async fn test_dml_limit_cap_none_limit_unchanged() {
        use crate::session_context::{current_dml_limit_cap, with_dml_limit_cap};

        // When there is no LIMIT clause (None), the cap should not inject one.
        with_dml_limit_cap(100, async {
            let mut limit: Option<usize> = None;
            if let Some(ref mut l) = limit {
                let cap = current_dml_limit_cap();
                if cap > 0 && *l > cap {
                    *l = cap;
                }
            }
            assert_eq!(limit, None);
        })
        .await;
    }
}
