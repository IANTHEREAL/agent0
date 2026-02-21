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
use crate::sql::query_context::QueryContext;
use crate::types::{Row, TableSchema};

use anyhow::Result;
use std::collections::HashMap;
use tikv_client::Transaction;

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
        let subqueries_materialized = self
            .pre_materialize_async_exprs(
                &substituted,
                txn,
                db_id,
                sequence_values,
                search_path,
                ctes,
            )
            .await?;

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
