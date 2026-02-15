//! Query execution helpers

use super::*;

impl Executor {
    pub(crate) async fn eval_expr_maybe_sequence(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        expr: &Expr,
        row: Option<&Row>,
        schema: Option<&TableSchema>,
    ) -> Result<Value> {
        if sequences::expr_needs_async_eval(expr) {
            sequences::eval_expr_with_sequences(
                &self.store,
                txn,
                db_id,
                sequence_values,
                search_path,
                expr,
                row,
                schema,
            )
            .await
        } else {
            match (row, schema) {
                (Some(r), Some(s)) => {
                    let alias = s.name.rsplit('.').next().unwrap_or(&s.name);
                    crate::sql::expr::bridge::eval_ast_expr_with_row(expr, r, s, alias)
                }
                _ => crate::sql::expr::bridge::eval_const_ast_expr(expr),
            }
        }
    }

    pub(crate) async fn eval_expr_join_maybe_sequence(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        expr: &Expr,
        combined_row: &Row,
        tables: &[(&str, &TableSchema)],
    ) -> Result<Value> {
        if sequences::expr_needs_async_eval(expr) {
            sequences::eval_expr_join_with_sequences(
                &self.store,
                txn,
                db_id,
                sequence_values,
                search_path,
                expr,
                combined_row,
                tables,
            )
            .await
        } else {
            crate::sql::expr::bridge::eval_ast_expr_with_join_row(expr, combined_row, tables)
        }
    }

    pub(crate) async fn execute_query(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        query: &Query,
    ) -> Result<ExecuteResult> {
        let ctes = self
            .build_cte_context(txn, db_id, sequence_values, search_path, query)
            .await?;
        self.execute_query_with_ctes(txn, db_id, sequence_values, search_path, query, &ctes)
            .await
    }
}
