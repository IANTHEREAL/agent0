//! Prepared statement analysis: freeze semantics at Parse time.
//!
//! `Executor::analyze_for_prepared()` runs the same pipeline as execution
//! (view expansion → catalog snapshot → Analyzer → rewriter) but returns
//! the frozen IR instead of executing it.  The protocol handler wraps
//! the result into `PreparedStatement` / `PreparedExec`.

use super::catalog_prefetch::{build_catalog_snapshot, build_catalog_snapshot_for_statement};
use super::view_rewrite::expand_views_in_query;
use super::*;
use crate::sql::analyzer::types::{AnalyzedQuery, AnalyzedStatement};
use crate::sql::analyzer::Analyzer;
use crate::sql::error::SqlError;
use crate::sql::names::normalize_ident;

/// Result of analyzing a SQL statement for prepared execution.
pub enum PreparedAnalysis {
    /// SELECT / set operation / VALUES — analyzed query IR.
    Query {
        analyzed: AnalyzedQuery,
        locks: Vec<sqlparser::ast::LockClause>,
        select_into: Option<sqlparser::ast::SelectInto>,
        output_schema: Vec<(String, DataType)>,
        param_types: Vec<DataType>,
        base_table_names: Vec<String>,
        table_versions: Vec<(String, u64)>,
        has_recursive_cte: bool,
    },
    /// INSERT / UPDATE / DELETE — analyzed DML IR.
    Dml {
        analyzed: AnalyzedStatement,
        output_schema: Vec<(String, DataType)>,
        param_types: Vec<DataType>,
        table_versions: Vec<(String, u64)>,
    },
    /// DDL / utility / non-analyzable statement.
    Utility,
}

impl Executor {
    /// Analyze a SQL statement at Parse time to produce frozen execution IR.
    ///
    /// Uses the same pipeline shape as runtime execution:
    /// - SELECT: view expansion → catalog snapshot → Analyzer → rewriter
    /// - DML: catalog snapshot → Analyzer
    ///
    /// Returns `PreparedAnalysis` with the analyzed IR and finalized parameter types.
    /// The protocol handler constructs `PreparedStatement` from this.
    pub async fn analyze_for_prepared(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        search_path: &[String],
        sql: &str,
        param_count: usize,
        client_oids: &[Option<DataType>],
    ) -> Result<PreparedAnalysis> {
        let statements = parse_sql(sql)?;
        if statements.is_empty() {
            return Ok(PreparedAnalysis::Utility);
        }
        let stmt = &statements[0];

        match stmt {
            Statement::Query(query) => {
                // Pipeline matches analyze_rewrite.rs + select/analyzed/mod.rs:
                // 1. View expansion
                let expanded =
                    expand_views_in_query(self.store().as_ref(), txn, db_id, search_path, query)
                        .await?;

                // 2. Build CTE schema context for analysis.
                // For WITH RECURSIVE, infer the self-reference schema from the
                // non-recursive arm so Parse-time analysis can resolve recursive
                // table references without executing the CTE.
                let analysis_ctes = self
                    .build_prepared_cte_schemas(
                        txn,
                        db_id,
                        search_path,
                        &expanded,
                        param_count,
                        client_oids,
                    )
                    .await?;

                // 3. Catalog snapshot with inferred CTE schemas.
                let catalog = build_catalog_snapshot(
                    self.store().as_ref(),
                    txn,
                    db_id,
                    search_path,
                    self.tenant_keyspace(),
                    &expanded,
                    &analysis_ctes,
                )
                .await?;

                // 4. Analyzer with parameter context
                let mut analyzer = Analyzer::new_with_params(&catalog, param_count, client_oids);
                let analyzed = stacker::maybe_grow(128 * 1024 * 1024, 256 * 1024 * 1024, || {
                    analyzer.analyze_query(&expanded)
                })
                .map_err(SqlError::from)?;

                // 5. Rewriter
                let rewritten = stacker::maybe_grow(128 * 1024 * 1024, 256 * 1024 * 1024, || {
                    crate::sql::rewriter::rewrite_query(analyzed)
                });

                // 6. Finalize parameter types
                let param_types = analyzer.finalize_param_types().map_err(SqlError::from)?;
                let output_schema = rewritten.output_schema.clone();

                // 7. Collect base table names for RBAC
                let base_table_names = catalog
                    .base_table_full_names()
                    .into_iter()
                    .map(|s| s.to_string())
                    .collect();
                let table_versions = catalog.base_table_versions();

                // 8. Extract locks + SELECT INTO from original AST
                let locks = query.locks.clone();
                let select_into = match &*query.body {
                    SetExpr::Select(s) => s.into.clone(),
                    _ => None,
                };
                let has_recursive_cte = query_contains_recursive_cte(&expanded);

                crate::sql::stack_safety::drop_on_grown_stack(expanded);

                Ok(PreparedAnalysis::Query {
                    analyzed: rewritten,
                    locks,
                    select_into,
                    output_schema,
                    param_types,
                    base_table_names,
                    table_versions,
                    has_recursive_cte,
                })
            }

            Statement::Insert { .. } | Statement::Update { .. } | Statement::Delete { .. } => {
                // Pipeline matches stmt_dml.rs
                let catalog = build_catalog_snapshot_for_statement(
                    self.store().as_ref(),
                    txn,
                    db_id,
                    search_path,
                    self.tenant_keyspace(),
                    stmt,
                )
                .await?;

                let mut analyzer = Analyzer::new_with_params(&catalog, param_count, client_oids);
                let analyzed_stmt = analyzer.analyze_statement(stmt).map_err(SqlError::from)?;
                let param_types = analyzer.finalize_param_types().map_err(SqlError::from)?;

                let output_schema = match &analyzed_stmt {
                    AnalyzedStatement::Query(q) => q.output_schema.clone(),
                    AnalyzedStatement::Insert(i) => returning_schema(&i.returning),
                    AnalyzedStatement::Update(u) => returning_schema(&u.returning),
                    AnalyzedStatement::Delete(d) => returning_schema(&d.returning),
                };
                let table_versions = catalog.base_table_versions();

                Ok(PreparedAnalysis::Dml {
                    analyzed: analyzed_stmt,
                    output_schema,
                    param_types,
                    table_versions,
                })
            }

            _ => {
                if param_count > 0 {
                    Err(SqlError::InvalidParameterUsage {
                        index: 1,
                        context: "utility statements do not support parameters".to_string(),
                    }
                    .into())
                } else {
                    Ok(PreparedAnalysis::Utility)
                }
            }
        }
    }

    async fn build_prepared_cte_schemas(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        search_path: &[String],
        query: &Query,
        param_count: usize,
        client_oids: &[Option<DataType>],
    ) -> Result<HashMap<String, (TableSchema, Vec<Row>)>> {
        let mut ctes: HashMap<String, (TableSchema, Vec<Row>)> = HashMap::new();
        let Some(with) = &query.with else {
            return Ok(ctes);
        };

        for cte in &with.cte_tables {
            let cte_name = cte.alias.name.value.to_lowercase();
            let schema_query = if with.recursive
                && crate::sql::executor::cte::cte_is_recursive(&cte.query, &cte_name)
            {
                let (base_expr, _, _) =
                    crate::sql::executor::cte::decompose_recursive_union(&cte.query, &cte_name)?;
                Query {
                    with: None,
                    body: base_expr,
                    order_by: vec![],
                    limit: None,
                    offset: None,
                    fetch: None,
                    locks: vec![],
                    limit_by: vec![],
                    for_clause: None,
                }
            } else {
                cte.query.as_ref().clone()
            };
            let output_schema = self
                .analyze_query_output_schema(
                    txn,
                    db_id,
                    search_path,
                    &schema_query,
                    &ctes,
                    param_count,
                    client_oids,
                )
                .await?;
            let columns: Vec<(String, DataType)> = if cte.alias.columns.is_empty() {
                output_schema
            } else {
                cte.alias
                    .columns
                    .iter()
                    .zip(output_schema.iter())
                    .map(|(alias_col, (_, dt))| (normalize_ident(alias_col), dt.clone()))
                    .collect()
            };
            let table_schema =
                crate::sql::executor::cte::build_cte_table_schema(&cte_name, columns);
            ctes.insert(cte_name, (table_schema, vec![]));
        }

        Ok(ctes)
    }

    async fn analyze_query_output_schema(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        search_path: &[String],
        query: &Query,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
        param_count: usize,
        client_oids: &[Option<DataType>],
    ) -> Result<Vec<(String, DataType)>> {
        let expanded =
            expand_views_in_query(self.store().as_ref(), txn, db_id, search_path, query).await?;
        let catalog = build_catalog_snapshot(
            self.store().as_ref(),
            txn,
            db_id,
            search_path,
            self.tenant_keyspace(),
            &expanded,
            ctes,
        )
        .await?;
        let mut analyzer = Analyzer::new_with_params(&catalog, param_count, client_oids);
        let analyzed = stacker::maybe_grow(128 * 1024 * 1024, 256 * 1024 * 1024, || {
            analyzer.analyze_query(&expanded)
        })
        .map_err(SqlError::from)?;
        crate::sql::stack_safety::drop_on_grown_stack(expanded);
        Ok(analyzed.output_schema)
    }
}

fn returning_schema(
    ret: &Option<Vec<crate::sql::analyzer::types::AnalyzedProjection>>,
) -> Vec<(String, DataType)> {
    match ret {
        Some(projections) => projections
            .iter()
            .map(|p| (p.output_name.clone(), p.expr.data_type.clone()))
            .collect(),
        None => vec![],
    }
}

fn query_contains_recursive_cte(query: &Query) -> bool {
    struct RecursiveCteDetector {
        has_recursive_cte: bool,
    }

    impl Visitor for RecursiveCteDetector {
        type Break = ();

        fn pre_visit_query(&mut self, q: &Query) -> ControlFlow<Self::Break> {
            if let Some(with) = &q.with {
                for cte in &with.cte_tables {
                    let cte_name = cte.alias.name.value.to_lowercase();
                    if with.recursive
                        && crate::sql::executor::cte::cte_is_recursive(&cte.query, &cte_name)
                    {
                        self.has_recursive_cte = true;
                        return ControlFlow::Break(());
                    }
                }
            }
            ControlFlow::Continue(())
        }
    }

    let mut detector = RecursiveCteDetector {
        has_recursive_cte: false,
    };
    let _ = query.visit(&mut detector);
    detector.has_recursive_cte
}

#[cfg(test)]
mod tests {
    use super::query_contains_recursive_cte;
    use crate::sql::parse_sql;
    use sqlparser::ast::Statement;

    fn parse_query(sql: &str) -> sqlparser::ast::Query {
        let mut stmts = parse_sql(sql).expect("parse sql");
        let stmt = stmts.remove(0);
        let Statement::Query(query) = stmt else {
            panic!("expected query");
        };
        *query
    }

    #[test]
    fn detects_recursive_cte_in_root_query() {
        let query = parse_query(
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 3) \
             SELECT * FROM t",
        );
        assert!(query_contains_recursive_cte(&query));
    }

    #[test]
    fn ignores_non_recursive_cte() {
        let query = parse_query("WITH t AS (SELECT 1) SELECT * FROM t");
        assert!(!query_contains_recursive_cte(&query));
    }

    #[test]
    fn detects_recursive_cte_in_nested_query() {
        let query = parse_query(
            "SELECT * FROM (WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 2) SELECT * FROM t) s",
        );
        assert!(query_contains_recursive_cte(&query));
    }
}
