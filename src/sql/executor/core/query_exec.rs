//! Query execution helpers

use super::*;
use crate::sql::sequences::SequenceSession;

impl Executor {
    pub(crate) async fn execute_query(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut SequenceSession,
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

#[cfg(test)]
mod tests {
    use super::Executor;

    #[test]
    fn execute_query_signature_is_stable() {
        let _f = Executor::execute_query;
        let _ = _f;
    }
}
