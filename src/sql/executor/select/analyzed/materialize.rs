//! Execution-time materialization for typed expressions.
//!
//! The typed evaluator (`typed_eval`) is pure and cannot execute subqueries or
//! consult catalogs/transactions. The analyzed executor resolves those nodes
//! before evaluation via this module.
//!
//! Orchestrates three materialization phases:
//! 1. Correlated outer-ref substitution (scope_depth -> constants)
//! 2. Uncorrelated subquery execution (IN/EXISTS/Scalar/Array/ANYALL)
//! 3. Catalog-dependent function resolution (delegated to `materialize_catalog`)

use crate::sql::analyzer::types::TypedExpr;
use crate::sql::executor::core::Executor;
use crate::sql::expr::classify::needs_pre_materialization;
use crate::sql::query_context::QueryContext;
use crate::types::{Row, TableSchema};

use anyhow::Result;
use std::collections::HashMap;
use tikv_client::Transaction;

enum PreMaterializeDecision {
    Skip(TypedExpr),
    Run(TypedExpr),
}

fn decide_pre_materialize_subqueries(substituted: TypedExpr) -> PreMaterializeDecision {
    if needs_pre_materialization(&substituted) {
        PreMaterializeDecision::Run(substituted)
    } else {
        PreMaterializeDecision::Skip(substituted)
    }
}

impl Executor {
    /// Materialize a typed expression for evaluation on a specific row.
    ///
    /// This resolves:
    /// - correlated references via substitution (scope_depth -> constants)
    /// - uncorrelated subqueries via execution (IN/EXISTS/Scalar/Array/ANYALL)
    /// - catalog-dependent functions (pg_get_indexdef, format_type, ...)
    pub(crate) async fn materialize_expr_for_row(
        &self,
        expr: &TypedExpr,
        row: &Row,
        correlated_outer_row: Option<&Row>,
        schema: Option<&TableSchema>,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
        qctx: &QueryContext,
    ) -> Result<TypedExpr> {
        // 1) Substitute correlated outer refs (scope_depth > 0) to constants.
        let outer_row = correlated_outer_row.unwrap_or(row);
        let substituted = super::subquery::substitute_outer_refs_in_expr(expr, outer_row);

        // 2) Resolve any now-uncorrelated subqueries.
        let subqueries_materialized = match decide_pre_materialize_subqueries(substituted) {
            PreMaterializeDecision::Run(expr) => {
                self.pre_materialize_async_exprs_prechecked(
                    &expr,
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    ctes,
                )
                .await?
            }
            PreMaterializeDecision::Skip(expr) => expr,
        };

        // 3) Resolve catalog-dependent functions.
        self.materialize_catalog_functions(
            &subqueries_materialized,
            row,
            schema,
            txn,
            db_id,
            sequence_values,
            search_path,
            qctx,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::analyzer::types::{AnalyzedQuery, AnalyzedQueryBody, TypedExprKind};
    use crate::types::{DataType, Value};

    fn const_bool(v: bool) -> TypedExpr {
        TypedExpr::new(
            TypedExprKind::Constant(Value::Boolean(v)),
            DataType::Boolean,
        )
    }

    fn scalar_subquery_expr() -> TypedExpr {
        let subquery = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Values(vec![vec![TypedExpr::new(
                TypedExprKind::Constant(Value::Int32(1)),
                DataType::Int32,
            )]]),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![("v".to_string(), DataType::Int32)],
        };
        TypedExpr::new(
            TypedExprKind::ScalarSubquery(Box::new(subquery)),
            DataType::Int32,
        )
    }

    #[test]
    fn decide_pre_materialize_subqueries_skips_non_async_expr() {
        let expr = const_bool(true);
        let decision = decide_pre_materialize_subqueries(expr);
        match decision {
            PreMaterializeDecision::Skip(TypedExpr {
                kind: TypedExprKind::Constant(Value::Boolean(true)),
                ..
            }) => {}
            PreMaterializeDecision::Skip(other) => {
                panic!(
                    "expected boolean constant passthrough, got {:?}",
                    other.kind
                )
            }
            PreMaterializeDecision::Run(_) => {
                panic!("non-async expression must not trigger pre-materialization")
            }
        }
    }

    #[test]
    fn decide_pre_materialize_subqueries_routes_subquery_expr() {
        let expr = scalar_subquery_expr();
        let decision = decide_pre_materialize_subqueries(expr);
        match decision {
            PreMaterializeDecision::Run(TypedExpr {
                kind: TypedExprKind::ScalarSubquery(_),
                ..
            }) => {}
            PreMaterializeDecision::Run(other) => {
                panic!(
                    "expected scalar-subquery pre-materialization route, got {:?}",
                    other.kind
                )
            }
            PreMaterializeDecision::Skip(_) => {
                panic!("subquery expression must trigger pre-materialization")
            }
        }
    }
}
