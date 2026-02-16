//! Query execution helpers

use super::*;

impl Executor {
    pub(crate) async fn execute_query(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        query: &Query,
        current_role: Option<&str>,
    ) -> Result<ExecuteResult> {
        let ctes = self
            .build_cte_context(
                txn,
                db_id,
                sequence_values,
                search_path,
                query,
                current_role,
            )
            .await?;
        self.execute_query_with_ctes(
            txn,
            db_id,
            sequence_values,
            search_path,
            query,
            &ctes,
            current_role,
        )
        .await
    }
}
