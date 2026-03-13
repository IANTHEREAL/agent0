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
//! 5) post-analysis rewriter (flattens view subqueries)
//! 6) RLS predicate injection (wraps tables in security barrier subqueries)
//!
//! Contract: this is the only semantic entrypoint. Callers must not implement
//! runtime fallback to alternate planning/execution paths on analysis failure.

use super::catalog_prefetch::build_catalog_snapshot;
use super::view_rewrite::expand_views_in_query;
use super::*;
use crate::auth::Privilege;
use crate::model::{RlsCommand, RlsPolicy};
use crate::sql::analyzer::{AnalyzedQuery, Analyzer};
use crate::sql::error::SqlError;
use crate::sql::rls::{inject_rls_predicates, should_bypass_rls};

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

        // Check if extended-query parameters are active (QUERY_PARAMS task-local).
        // When present, create a param-aware Analyzer so $N placeholders resolve
        // to TypedExprKind::Parameter nodes instead of failing.
        let query_params = crate::sql::query_context::QueryContext::current_query_params();
        let mut analyzer = if !query_params.is_empty() {
            let param_types = crate::sql::query_context::QueryContext::current_query_param_types();
            let client_oids = if param_types.len() == query_params.len() {
                param_types
            } else {
                vec![None; query_params.len()]
            };
            Analyzer::new_with_params(&catalog, query_params.len(), &client_oids)
        } else {
            Analyzer::new(&catalog)
        };
        let analyzed = stacker::maybe_grow(128 * 1024 * 1024, 256 * 1024 * 1024, || {
            analyzer.analyze_query(&expanded_query)
        })
        .map_err(SqlError::from)?;

        // --- Post-analysis rewrite (step 5) ---
        // Flatten simple view subqueries before RLS injection so that
        // base-table Table refs are exposed for wrapping.
        let rewritten = stacker::maybe_grow(128 * 1024 * 1024, 256 * 1024 * 1024, || {
            crate::sql::rewriter::rewrite_query(analyzed)
        });

        // --- RLS predicate injection (step 6) ---
        // After rewriting (so flattened tables are visible) but before the
        // optimizer, wrap RLS-filtered tables in security barrier subqueries.
        // These subqueries must not be flattened — running the rewriter first
        // guarantees they survive to the optimizer where Subquery is a
        // pushdown barrier.
        let rewritten = self
            .maybe_inject_rls_select(txn, db_id, search_path, rewritten, current_role, &catalog)
            .await?;
        // Drop deep expanded AST on a grown stack before returning.
        crate::sql::stack_safety::drop_on_grown_stack(expanded_query);
        Ok(rewritten)
    }

    /// Inject RLS predicates if any referenced table has row-level security enabled.
    ///
    /// Fast path: if no table has `rls_enabled`, returns `analyzed` unchanged.
    ///
    /// **Prepared statement contract**: This method is intentionally NOT called from
    /// `prepared_analysis.rs`. For prepared statements on RLS-sensitive tables,
    /// the `rls_sensitive` flag (see #1811) triggers a text fallback at EXECUTE time,
    /// which re-enters `analyze_then_rewrite_query()` where this injection runs
    /// naturally. Do not duplicate this call into the prepared analysis path.
    async fn maybe_inject_rls_select(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        _search_path: &[String],
        analyzed: AnalyzedQuery,
        current_role: Option<&str>,
        catalog: &crate::sql::analyzer::CatalogSnapshot,
    ) -> Result<AnalyzedQuery> {
        let table_schemas = catalog.base_table_schemas();

        // Fast path: skip if no table has RLS enabled.
        let any_rls = table_schemas.values().any(|s| s.rls_enabled);
        if !any_rls {
            return Ok(analyzed);
        }

        let role = current_role.unwrap_or(""); // empty role matches no role-specific policies
        let is_superuser = crate::extensions::context::is_superuser();
        let bypass_rls = crate::extensions::context::bypass_rls();

        // Load RLS policies for all RLS-enabled tables.
        let mut policies_by_table: HashMap<u64, Vec<RlsPolicy>> = HashMap::new();
        for schema in table_schemas.values() {
            if !schema.rls_enabled {
                continue;
            }
            if should_bypass_rls(
                is_superuser,
                bypass_rls,
                role,
                &schema.owner,
                schema.rls_enabled,
                schema.rls_force,
            ) {
                continue;
            }
            let policies = if let Some(cached) =
                self.rls_policy_cache()
                    .get(db_id, schema.table_id, schema.version)
            {
                cached
            } else {
                let loaded = self
                    .store()
                    .list_policies_for_table(txn, db_id, schema.table_id)
                    .await?;
                self.rls_policy_cache()
                    .put(db_id, schema.table_id, schema.version, loaded.clone());
                loaded
            };
            if !policies.is_empty() {
                policies_by_table.insert(schema.table_id, policies);
            }
            // If no policies but RLS is enabled, leave the table absent from the map.
            // inject_rls_predicates will produce WHERE false (default-deny).
        }

        // Build a minimal QueryContext for policy expression compilation.
        let qctx = crate::sql::query_context::QueryContext::new(
            0, // connection_id not critical for RLS expr compilation
            std::sync::Arc::from(""),
            std::sync::Arc::from(role),
            chrono::Utc::now().timestamp_millis(),
            chrono::Utc::now().timestamp_millis(),
            std::sync::Arc::from("UTC"),
        );

        let rls_ctx = crate::sql::rls::inject::RlsContext {
            current_role: role,
            is_superuser,
            bypass_rls,
            table_schemas: &table_schemas,
            policies_by_table: &policies_by_table,
            qctx: &qctx,
            command: RlsCommand::Select,
            expr_cache: Some((self.rls_policy_cache(), db_id)),
        };

        inject_rls_predicates(analyzed, &rls_ctx)
    }
}
