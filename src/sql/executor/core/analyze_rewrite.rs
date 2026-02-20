//! Shared `SELECT/WITH` analyze+rewrite entry.
//!
//! This module is the single semantic entry point for:
//! - execution (`try_execute_analyzed`)
//! - `EXPLAIN SELECT/WITH`
//!
//! Pipeline (must stay identical for both):
//! 1) view expansion
//! 2) catalog snapshot build
//! 3) SELECT privilege check
//! 4) Analyzer
//! 5) post-analysis rewriter
//!
//! Contract: this is the only semantic entrypoint. Callers must not implement
//! runtime fallback to alternate planning/execution paths on analysis failure.

use super::catalog_prefetch::build_catalog_snapshot;
use super::view_rewrite::expand_views_in_query;
use super::*;
use crate::auth::Privilege;
use crate::sql::analyzer::{AnalyzedQuery, Analyzer};
use crate::sql::error::SqlError;

impl Executor {
    /// Canonical `SELECT/WITH` entry:
    /// raw query AST -> rewritten analyzed query.
    ///
    /// Errors are propagated directly to preserve single-path semantics.
    pub(crate) async fn analyze_then_rewrite_query(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        search_path: &[String],
        query: &Query,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
        current_role: Option<&str>,
    ) -> Result<AnalyzedQuery> {
        let expanded_query =
            expand_views_in_query(self.store().as_ref(), txn, db_id, search_path, query).await?;

        let catalog = build_catalog_snapshot(
            self.store().as_ref(),
            txn,
            db_id,
            search_path,
            self.tenant_keyspace(),
            &expanded_query,
            ctes,
        )
        .await?;

        if current_role.is_some() {
            for table_name in catalog.base_table_full_names() {
                self.require_table_privilege(txn, current_role, Privilege::Select, table_name)
                    .await?;
            }
        }

        let mut analyzer = Analyzer::new(&catalog);
        let analyzed = stacker::maybe_grow(128 * 1024 * 1024, 256 * 1024 * 1024, || {
            analyzer.analyze_query(&expanded_query)
        })
        .map_err(SqlError::from)?;
        let rewritten = stacker::maybe_grow(128 * 1024 * 1024, 256 * 1024 * 1024, || {
            crate::sql::rewriter::rewrite_query(analyzed)
        });
        // Drop deep expanded AST on a grown stack before returning.
        crate::sql::stack_safety::drop_on_grown_stack(expanded_query);
        Ok(rewritten)
    }
}
