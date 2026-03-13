//! CTE (Common Table Expression) execution for the SQL executor

use super::super::names::normalize_ident;
use super::super::ExecuteResult;
use super::core::Executor;
use crate::model::{ColumnDef, DataType, Row, TableSchema};
use crate::sql::binder::{extract_relation_references_from_query, RelationDep};
use crate::sql::error::SqlError;
use crate::sql::sequences::SequenceSession;
use anyhow::{anyhow, Result};
use sqlparser::ast::{Ident, Query, SetExpr, SetOperator, SetQuantifier, Visit, Visitor};
use std::collections::HashMap;
use std::future::Future;
use std::ops::ControlFlow;
use std::pin::Pin;
use tikv_client::Transaction;

const RECURSIVE_CTE_MAX_ITERATIONS: usize = 1000;

fn check_recursive_cte_iteration_limit(iteration: usize) -> Result<()> {
    if iteration == RECURSIVE_CTE_MAX_ITERATIONS {
        return Err(SqlError::StatementTooComplex {
            message: format!(
                "recursive query exceeded maximum iteration count ({RECURSIVE_CTE_MAX_ITERATIONS})"
            ),
        }
        .into());
    }
    Ok(())
}

fn merge_recursive_rows(
    all_rows: &mut Vec<Row>,
    new_rows: Vec<Row>,
    is_union_all: bool,
) -> Vec<Row> {
    if is_union_all {
        all_rows.extend(new_rows.clone());
        new_rows
    } else {
        let mut unique_new_rows = Vec::new();
        for row in new_rows {
            if !all_rows.contains(&row) {
                unique_new_rows.push(row.clone());
                all_rows.push(row);
            }
        }
        unique_new_rows
    }
}

impl Executor {
    pub(crate) async fn build_cte_context_with_base(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut SequenceSession,
        search_path: &[String],
        query: &Query,
        base_ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
        current_role: Option<&str>,
    ) -> Result<HashMap<String, (TableSchema, Vec<Row>)>> {
        let mut ctes: HashMap<String, (TableSchema, Vec<Row>)> = base_ctes.clone();
        if let Some(with) = &query.with {
            for cte in &with.cte_tables {
                let cte_name = normalize_ident(&cte.alias.name);

                if with.recursive && cte_is_recursive(&cte.query, &cte_name) {
                    let (schema, rows) = self
                        .execute_recursive_cte(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            &cte_name,
                            &cte.query,
                            &cte.alias.columns,
                            &ctes,
                            current_role,
                        )
                        .await?;
                    ctes.insert(cte_name, (schema, rows));
                } else {
                    let cte_result = self
                        .execute_query_with_outer_ctes(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            &cte.query,
                            &ctes,
                            current_role,
                        )
                        .await?;
                    match cte_result {
                        ExecuteResult::Select {
                            columns,
                            column_types,
                            rows,
                            timezone: _,
                        } => {
                            let col_names: Vec<String> = if cte.alias.columns.is_empty() {
                                columns
                            } else {
                                cte.alias.columns.iter().map(normalize_ident).collect()
                            };
                            let inferred_types: Vec<DataType> = if let Some(types) = column_types {
                                types
                            } else {
                                crate::model::infer_column_types_from_rows(&rows, col_names.len())
                            };
                            let schema = build_cte_table_schema(
                                &cte_name,
                                col_names
                                    .into_iter()
                                    .enumerate()
                                    .map(|(idx, n)| {
                                        let dt = inferred_types
                                            .get(idx)
                                            .cloned()
                                            .unwrap_or(DataType::Text);
                                        (n, dt)
                                    })
                                    .collect(),
                            );
                            ctes.insert(cte_name, (schema, rows));
                        }
                        _ => return Err(anyhow!("CTE must be a SELECT query")),
                    }
                }
            }
        }
        Ok(ctes)
    }

    pub(crate) async fn build_cte_context(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut SequenceSession,
        search_path: &[String],
        query: &Query,
        current_role: Option<&str>,
    ) -> Result<HashMap<String, (TableSchema, Vec<Row>)>> {
        let base = HashMap::new();
        self.build_cte_context_with_base(
            txn,
            db_id,
            sequence_values,
            search_path,
            query,
            &base,
            current_role,
        )
        .await
    }

    /// Build CTE runtime context for nested WITH queries contained inside `query`.
    ///
    /// This excludes the root query's own WITH clause (callers typically materialize
    /// root WITH separately) and only processes nested Query nodes. The merged map
    /// is additive over `base_ctes`, preserving outer-scope CTE visibility.
    #[allow(clippy::type_complexity)]
    pub(crate) fn build_nested_with_cte_context_with_base<'a>(
        &'a self,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut SequenceSession,
        search_path: &'a [String],
        query: &'a Query,
        base_ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
        current_role: Option<&'a str>,
    ) -> Pin<Box<dyn Future<Output = Result<HashMap<String, (TableSchema, Vec<Row>)>>> + Send + 'a>>
    {
        Box::pin(async move {
            let mut ctes = base_ctes.clone();
            for nested_query in collect_nested_with_queries(query) {
                ctes = self
                    .build_cte_context_with_base(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        &nested_query,
                        &ctes,
                        current_role,
                    )
                    .await?;
            }
            Ok(ctes)
        })
    }

    pub(crate) async fn execute_recursive_cte(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut SequenceSession,
        search_path: &[String],
        cte_name: &str,
        query: &Query,
        alias_columns: &[Ident],
        existing_ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
        current_role: Option<&str>,
    ) -> Result<(TableSchema, Vec<Row>)> {
        let (base_expr, recursive_expr, is_union_all) = decompose_recursive_union(query, cte_name)?;

        let base_query = Query {
            with: None,
            body: base_expr,
            order_by: vec![],
            limit: None,
            offset: None,
            fetch: None,
            locks: vec![],
            limit_by: vec![],
            for_clause: None,
        };
        let base_result = self
            .execute_query_with_ctes(
                txn,
                db_id,
                sequence_values,
                search_path,
                &base_query,
                existing_ctes,
                current_role,
            )
            .await?;
        let (columns, base_types, mut all_rows) = match base_result {
            ExecuteResult::Select {
                columns,
                column_types,
                rows,
                timezone: _,
            } => (columns, column_types, rows),
            _ => return Err(anyhow!("Recursive CTE base must be SELECT")),
        };

        let col_names: Vec<String> = if alias_columns.is_empty() {
            columns
        } else {
            alias_columns.iter().map(normalize_ident).collect()
        };
        let inferred_types: Vec<DataType> = if let Some(types) = base_types {
            types
        } else {
            crate::model::infer_column_types_from_rows(&all_rows, col_names.len())
        };
        let schema = build_cte_table_schema(
            cte_name,
            col_names
                .into_iter()
                .enumerate()
                .map(|(idx, n)| {
                    let dt = inferred_types.get(idx).cloned().unwrap_or(DataType::Text);
                    (n, dt)
                })
                .collect(),
        );

        let mut working_table = all_rows.clone();
        let mut iteration = 0;

        while !working_table.is_empty() {
            let mut temp_ctes = existing_ctes.clone();
            temp_ctes.insert(
                cte_name.to_string(),
                (schema.clone(), working_table.clone()),
            );

            let recursive_query = Query {
                with: None,
                body: recursive_expr.clone(),
                order_by: vec![],
                limit: None,
                offset: None,
                fetch: None,
                locks: vec![],
                limit_by: vec![],
                for_clause: None,
            };
            let recursive_result = self
                .execute_query_with_ctes(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    &recursive_query,
                    &temp_ctes,
                    current_role,
                )
                .await?;

            let new_rows = match recursive_result {
                ExecuteResult::Select { rows, .. } => rows,
                _ => return Err(anyhow!("Recursive CTE iteration must be SELECT")),
            };

            if is_union_all {
                if new_rows.is_empty() {
                    break;
                }

                // db9 divergence: PostgreSQL does not impose a built-in recursive CTE depth cap;
                // unbounded recursion is typically constrained by statement_timeout. db9 enforces
                // a hard cap of 1000 iterations intentionally to prevent runaway recursive queries
                // from exhausting memory.
                //
                // For UNION ALL, the recursive step continues whenever the arm yields rows.
                check_recursive_cte_iteration_limit(iteration)?;
                iteration += 1;

                working_table = merge_recursive_rows(&mut all_rows, new_rows, true);
            } else {
                // For UNION (DISTINCT), deduplicate against accumulated result rows first.
                // If this iteration contributes no new distinct rows, recursion terminates
                // naturally and must not consume/violate the iteration cap.
                let deduped_new_rows = merge_recursive_rows(&mut all_rows, new_rows, false);
                if deduped_new_rows.is_empty() {
                    break;
                }

                check_recursive_cte_iteration_limit(iteration)?;
                iteration += 1;
                working_table = deduped_new_rows;
            }
        }

        Ok((schema, all_rows))
    }
}

fn set_expr_as_query(expr: &SetExpr) -> Query {
    Query {
        with: None,
        body: Box::new(expr.clone()),
        order_by: vec![],
        limit: None,
        offset: None,
        fetch: None,
        locks: vec![],
        limit_by: vec![],
        for_clause: None,
    }
}

/// Decompose a recursive CTE query into its base (non-recursive) and recursive arms.
///
/// Returns `(base_expr, recursive_expr, is_union_all)`.
/// Errors with `SqlError::Unsupported` if:
/// - The query body is not a UNION
/// - Both arms reference `cte_name` (both recursive)
/// - Neither arm references `cte_name` (both non-recursive)
pub(crate) fn decompose_recursive_union(
    query: &Query,
    cte_name: &str,
) -> Result<(Box<SetExpr>, Box<SetExpr>, bool)> {
    let (left, right, is_union_all) = match &*query.body {
        SetExpr::SetOperation {
            op: SetOperator::Union,
            set_quantifier,
            left,
            right,
        } => {
            let is_all = matches!(set_quantifier, SetQuantifier::All);
            (left, right, is_all)
        }
        _ => {
            return Err(SqlError::Unsupported(
                "recursive CTE must use UNION or UNION ALL".to_string(),
            )
            .into())
        }
    };

    let left_refs_self = set_expr_references_table(left, cte_name);
    let right_refs_self = set_expr_references_table(right, cte_name);
    match (left_refs_self, right_refs_self) {
        (false, true) => Ok((left.clone(), right.clone(), is_union_all)),
        (true, false) => Ok((right.clone(), left.clone(), is_union_all)),
        _ => Err(SqlError::Unsupported(
            "recursive CTE must have one non-recursive UNION arm".to_string(),
        )
        .into()),
    }
}

/// Build an ephemeral `TableSchema` for a CTE from resolved column names and types.
///
/// Produces a schema with `table_id: 0`, all columns nullable, no PK/indexes.
pub(crate) fn build_cte_table_schema(
    cte_name: &str,
    columns: Vec<(String, DataType)>,
) -> TableSchema {
    TableSchema {
        table_id: 0,
        name: cte_name.to_string(),
        columns: columns
            .into_iter()
            .map(|(name, data_type)| ColumnDef {
                name,
                data_type,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                generation_expr: None,
                generation_expr_authorized_by: None,
                collation: None,
            })
            .collect(),
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![],
        version: 1,
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        rls_enabled: false,
        rls_force: false,
        from_alias: None,
    }
}

/// Check if a CTE is recursive (references itself in the UNION)
pub(crate) fn cte_is_recursive(query: &Query, cte_name: &str) -> bool {
    match query.body.as_ref() {
        SetExpr::SetOperation { left, right, .. } => {
            set_expr_references_table(left, cte_name) || set_expr_references_table(right, cte_name)
        }
        SetExpr::Query(q) => cte_is_recursive(q, cte_name),
        _ => false,
    }
}

/// Check if a SetExpr references a specific table
pub(crate) fn set_expr_references_table(expr: &SetExpr, table_name: &str) -> bool {
    let target = table_name.to_string();
    let query = set_expr_as_query(expr);
    extract_relation_references_from_query(&query)
        .into_iter()
        .any(|dep| matches!(dep, RelationDep::Unqualified { name } if name == target))
}

/// Collect nested Query nodes (excluding root) that contain a WITH clause.
fn collect_nested_with_queries(query: &Query) -> Vec<Query> {
    struct NestedWithCollector {
        seen_root: bool,
        queries: Vec<Query>,
    }

    impl Visitor for NestedWithCollector {
        type Break = ();

        fn pre_visit_query(&mut self, q: &Query) -> ControlFlow<()> {
            if self.seen_root {
                if q.with.is_some() {
                    self.queries.push(q.clone());
                }
            } else {
                self.seen_root = true;
            }
            ControlFlow::Continue(())
        }
    }

    let mut collector = NestedWithCollector {
        seen_root: false,
        queries: Vec::new(),
    };
    let _ = query.visit(&mut collector);
    collector.queries
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Value;
    use crate::sql::parse_sql;
    use sqlparser::ast::Statement;

    fn parse_query(sql: &str) -> Query {
        let mut stmts = parse_sql(sql).expect("parse sql");
        let stmt = stmts.remove(0);
        let Statement::Query(query) = stmt else {
            panic!("expected query");
        };
        *query
    }

    /// Extract the CTE query from a WITH RECURSIVE statement.
    fn cte_query(sql: &str) -> Query {
        let query = parse_query(sql);
        let cte = &query.with.as_ref().unwrap().cte_tables[0];
        cte.query.as_ref().clone()
    }

    fn row_first_i64(row: &Row) -> i64 {
        match row.values.first().expect("row must have one column") {
            Value::Int32(v) => i64::from(*v),
            Value::Int64(v) => *v,
            other => panic!("expected int value, got {other:?}"),
        }
    }

    // --- decompose_recursive_union tests ---

    #[test]
    fn decompose_right_recursive() {
        let cte_q = cte_query(
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t) SELECT * FROM t",
        );
        let (base, recursive, is_union_all) = decompose_recursive_union(&cte_q, "t").unwrap();
        assert!(is_union_all);
        // Base arm should NOT reference t
        assert!(!set_expr_references_table(&base, "t"));
        // Recursive arm should reference t
        assert!(set_expr_references_table(&recursive, "t"));
    }

    #[test]
    fn decompose_left_recursive() {
        let cte_q = cte_query(
            "WITH RECURSIVE t(n) AS (SELECT n + 1 FROM t UNION ALL SELECT 1) SELECT * FROM t",
        );
        let (base, recursive, is_union_all) = decompose_recursive_union(&cte_q, "t").unwrap();
        assert!(is_union_all);
        assert!(!set_expr_references_table(&base, "t"));
        assert!(set_expr_references_table(&recursive, "t"));
    }

    #[test]
    fn decompose_both_recursive_fails() {
        // Both arms reference t
        let cte_q = cte_query(
            "WITH RECURSIVE t(n) AS (SELECT n FROM t UNION ALL SELECT n + 1 FROM t) SELECT * FROM t",
        );
        let err = decompose_recursive_union(&cte_q, "t").unwrap_err();
        let sql_err = err
            .downcast_ref::<SqlError>()
            .expect("must be SqlError::Unsupported, not bare anyhow");
        assert_eq!(sql_err.sqlstate(), "0A000");
        assert!(
            err.to_string().contains("one non-recursive UNION arm"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn decompose_neither_recursive_fails() {
        // Neither arm references t — this helper doesn't gate on cte_is_recursive
        let cte_q =
            cte_query("WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT 2) SELECT * FROM t");
        let err = decompose_recursive_union(&cte_q, "t").unwrap_err();
        let sql_err = err
            .downcast_ref::<SqlError>()
            .expect("must be SqlError::Unsupported, not bare anyhow");
        assert_eq!(sql_err.sqlstate(), "0A000");
        assert!(
            err.to_string().contains("one non-recursive UNION arm"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn decompose_non_union_body_fails() {
        let cte_q = cte_query("WITH RECURSIVE t(n) AS (SELECT 1) SELECT * FROM t");
        let err = decompose_recursive_union(&cte_q, "t").unwrap_err();
        let sql_err = err
            .downcast_ref::<SqlError>()
            .expect("must be SqlError::Unsupported, not bare anyhow");
        assert_eq!(sql_err.sqlstate(), "0A000");
        assert!(
            err.to_string().contains("UNION or UNION ALL"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn decompose_union_distinct() {
        let cte_q = cte_query(
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION SELECT n + 1 FROM t) SELECT * FROM t",
        );
        let (_base, _recursive, is_union_all) = decompose_recursive_union(&cte_q, "t").unwrap();
        assert!(!is_union_all);
    }

    #[test]
    fn decompose_qualified_name_not_counted_as_self_reference() {
        let cte_q = cte_query(
            "WITH RECURSIVE t(n) AS (SELECT 1 FROM public.t UNION ALL SELECT n + 1 FROM t WHERE n < 3) SELECT * FROM t",
        );
        let (base, recursive, is_union_all) = decompose_recursive_union(&cte_q, "t").unwrap();
        assert!(is_union_all);
        assert!(!set_expr_references_table(&base, "t"));
        assert!(set_expr_references_table(&recursive, "t"));
    }

    #[test]
    fn decompose_function_named_like_cte_not_counted_as_self_reference() {
        let cte_q = cte_query(
            "WITH RECURSIVE t(n) AS (SELECT 1 FROM t(1) UNION ALL SELECT n + 1 FROM t WHERE n < 3) SELECT * FROM t",
        );
        let (base, recursive, is_union_all) = decompose_recursive_union(&cte_q, "t").unwrap();
        assert!(is_union_all);
        assert!(!set_expr_references_table(&base, "t"));
        assert!(set_expr_references_table(&recursive, "t"));
    }

    #[test]
    fn decompose_nested_join_self_reference_detected() {
        let cte_q = cte_query(
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT t.n FROM (t JOIN (SELECT 1) s ON true) j) SELECT * FROM t",
        );
        let (base, recursive, is_union_all) = decompose_recursive_union(&cte_q, "t").unwrap();
        assert!(is_union_all);
        assert!(!set_expr_references_table(&base, "t"));
        assert!(set_expr_references_table(&recursive, "t"));
    }

    /// Regression for #1022: a CTE whose recursive arm references the CTE name
    /// ONLY inside a nested WITH that shadows the outer name at an inner scope.
    ///
    /// The scope-aware binder suppresses the inner reference (it is shadowed),
    /// so `set_expr_references_table` returns `false` for both arms.
    /// `decompose_recursive_union` must therefore error — this CTE is not
    /// recursive and `execute_recursive_cte` is never called for it.
    ///
    /// If `set_expr_references_table` were NOT scope-aware it would see `shadow`
    /// referenced in the right arm (inside the inner WITH body) and wrongly
    /// return `true`, causing misclassification as a recursive CTE.
    #[test]
    fn decompose_nested_shadow_arm_not_self_ref() {
        let cte_q = cte_query(
            "WITH RECURSIVE shadow AS ( \
                 SELECT 0 AS val \
                 UNION ALL \
                 SELECT sub.val + 1 \
                 FROM ( \
                     WITH shadow AS (SELECT 1 AS val) \
                     SELECT val FROM shadow \
                 ) sub \
                 WHERE sub.val > 100 \
             ) SELECT * FROM shadow",
        );
        // Left arm: SELECT 0 AS val — no table reference at all.
        let SetExpr::SetOperation { left, right, .. } = cte_q.body.as_ref() else {
            panic!("expected UNION body");
        };
        assert!(
            !set_expr_references_table(left, "shadow"),
            "left arm must not reference shadow"
        );
        // Right arm: shadow is referenced only inside a nested WITH that re-defines
        // shadow at an inner scope. The scope-aware binder suppresses this reference.
        assert!(
            !set_expr_references_table(right, "shadow"),
            "right arm must not reference shadow (inner WITH shadows it)"
        );

        // With both arms returning false, decompose_recursive_union cannot identify
        // a recursive arm and must error — the same path taken for non-recursive CTEs.
        let err = decompose_recursive_union(&cte_q, "shadow").unwrap_err();
        let sql_err = err
            .downcast_ref::<SqlError>()
            .expect("must be SqlError::Unsupported");
        assert_eq!(sql_err.sqlstate(), "0A000");
        assert!(
            err.to_string().contains("one non-recursive UNION arm"),
            "expected non-recursive error, got: {err}"
        );
    }

    // --- recursive iteration limit regression tests ---

    // db9 divergence: PG has no max_recursion_depth setting; infinite recursive CTEs hit statement_timeout.
    // db9 enforces a hard iteration cap (1000) as a resource protection measure.
    #[test]
    fn recursive_cte_limit_hit_returns_typed_sqlstate_and_error_message() {
        let mut all_rows = vec![Row::new(vec![Value::Int32(1)])];
        let mut working_table = all_rows.clone();
        let mut iteration = 0usize;

        loop {
            let new_rows: Vec<Row> = working_table
                .iter()
                .map(|row| Row::new(vec![Value::Int64(row_first_i64(row) + 1)]))
                .collect();
            if new_rows.is_empty() {
                panic!("infinite recursive arm simulation unexpectedly returned no rows");
            }

            match check_recursive_cte_iteration_limit(iteration) {
                Ok(()) => {}
                Err(err) => {
                    let sql_err = err
                        .downcast_ref::<SqlError>()
                        .expect("must be typed SqlError, not bare anyhow");
                    assert_eq!(sql_err.sqlstate(), "54001");
                    assert!(
                        err.to_string()
                            .contains("recursive query exceeded maximum iteration count"),
                        "unexpected error: {err}"
                    );
                    assert_eq!(iteration, RECURSIVE_CTE_MAX_ITERATIONS);
                    return;
                }
            }
            iteration += 1;

            working_table = merge_recursive_rows(&mut all_rows, new_rows, true);
        }
    }

    // db9 divergence: PG has no max_recursion_depth setting; infinite recursive CTEs hit statement_timeout.
    // db9 enforces a hard iteration cap (1000) as a resource protection measure.
    #[test]
    fn recursive_cte_999_iterations_returns_full_results() {
        let mut all_rows = vec![Row::new(vec![Value::Int32(1)])];
        let mut working_table = all_rows.clone();
        let mut iteration = 0usize;

        while !working_table.is_empty() {
            let new_rows: Vec<Row> = working_table
                .iter()
                .filter_map(|row| {
                    let n = row_first_i64(row);
                    (n < 999).then(|| Row::new(vec![Value::Int64(n + 1)]))
                })
                .collect();
            if new_rows.is_empty() {
                break;
            }

            check_recursive_cte_iteration_limit(iteration).unwrap();
            iteration += 1;
            working_table = merge_recursive_rows(&mut all_rows, new_rows, true);
        }

        assert_eq!(iteration, 998);
        assert_eq!(all_rows.len(), 999);
        for (idx, row) in all_rows.iter().enumerate() {
            assert_eq!(row_first_i64(row), (idx + 1) as i64);
        }
    }

    #[test]
    fn recursive_union_distinct_dedup_stops_before_cap() {
        let mut all_rows = vec![Row::new(vec![Value::Int32(1)])];
        let iteration = RECURSIVE_CTE_MAX_ITERATIONS;

        let new_rows = vec![Row::new(vec![Value::Int32(1)])];
        let deduped_new_rows = merge_recursive_rows(&mut all_rows, new_rows, false);
        assert!(deduped_new_rows.is_empty());
        assert!(check_recursive_cte_iteration_limit(iteration).is_err());

        // UNION (DISTINCT) must terminate on empty post-dedup working table and
        // therefore never evaluate the cap check in this case.
        if !deduped_new_rows.is_empty() {
            check_recursive_cte_iteration_limit(iteration).unwrap();
        }
    }

    // --- build_cte_table_schema tests ---

    #[test]
    fn build_schema_basic() {
        let schema = build_cte_table_schema(
            "my_cte",
            vec![
                ("col_a".to_string(), DataType::Int32),
                ("col_b".to_string(), DataType::Text),
            ],
        );
        assert_eq!(schema.table_id, 0);
        assert_eq!(schema.name, "my_cte");
        assert_eq!(schema.columns.len(), 2);
        assert_eq!(schema.columns[0].name, "col_a");
        assert_eq!(schema.columns[0].data_type, DataType::Int32);
        assert!(schema.columns[0].nullable);
        assert!(!schema.columns[0].primary_key);
        assert_eq!(schema.columns[1].name, "col_b");
        assert_eq!(schema.columns[1].data_type, DataType::Text);
        assert!(schema.pk_indices.is_empty());
        assert!(schema.indexes.is_empty());
    }

    #[test]
    fn build_schema_empty_columns() {
        let schema = build_cte_table_schema("empty", vec![]);
        assert_eq!(schema.columns.len(), 0);
        assert_eq!(schema.name, "empty");
    }

    // --- alias normalization regression test ---

    /// Regression: the prepared-analysis path previously used raw `.value.clone()`
    /// for CTE alias columns, preserving original case. PostgreSQL folds unquoted
    /// identifiers to lowercase. This test exercises the same alias → schema flow
    /// that `build_prepared_cte_schemas` uses: normalize aliases then feed into
    /// `build_cte_table_schema`.
    #[test]
    fn cte_alias_normalization_matches_pg() {
        use sqlparser::ast::Ident;

        // Simulate what prepared_analysis.rs:237-248 does:
        // 1. Parse alias columns from CTE definition
        // 2. normalize_ident each alias
        // 3. Zip with output_schema types
        // 4. Feed into build_cte_table_schema
        let alias_columns = [
            Ident::new("MyCol"),              // unquoted → should fold to "mycol"
            Ident::with_quote('"', "Quoted"), // quoted → should preserve "Quoted"
        ];
        let output_schema = [
            ("original_a".to_string(), DataType::Int32),
            ("original_b".to_string(), DataType::Text),
        ];

        let columns: Vec<(String, DataType)> = alias_columns
            .iter()
            .zip(output_schema.iter())
            .map(|(alias_col, (_, dt))| (normalize_ident(alias_col), dt.clone()))
            .collect();
        let schema = build_cte_table_schema("t", columns);

        assert_eq!(schema.columns[0].name, "mycol"); // PG-correct lowercase
        assert_eq!(schema.columns[1].name, "Quoted"); // quoted preserves case
        assert_eq!(schema.columns[0].data_type, DataType::Int32);
        assert_eq!(schema.columns[1].data_type, DataType::Text);
    }
}
