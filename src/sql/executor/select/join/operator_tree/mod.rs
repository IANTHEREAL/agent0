use super::super::analysis::projection_has_window_function;
use super::super::*;
use super::table_factor::{
    collect_visible_aliases_in_table_with_joins, duplicate_column_names_lowercase,
    extract_virtual_table_filter, TransparentNestedJoinInfo,
};
use super::using_merge::{build_coalesce_for_merge, rewrite_for_using_join, UsingMergeColumn};

mod projection;

const CORRELATED_SUBQUERY_JOIN_CONTEXT_UNSUPPORTED: &str =
    "Correlated subquery in JOIN context is not supported";

fn correlated_subquery_join_context_unsupported(mut outer_refs: Vec<String>) -> anyhow::Error {
    outer_refs.sort_by(|a, b| {
        a.to_lowercase()
            .cmp(&b.to_lowercase())
            .then_with(|| a.cmp(b))
    });
    outer_refs.dedup_by(|a, b| a.eq_ignore_ascii_case(b));

    let msg = if outer_refs.is_empty() {
        CORRELATED_SUBQUERY_JOIN_CONTEXT_UNSUPPORTED.to_string()
    } else {
        format!(
            "{CORRELATED_SUBQUERY_JOIN_CONTEXT_UNSUPPORTED} (outer refs: {})",
            outer_refs.join(", ")
        )
    };
    SqlError::Unsupported(msg).into()
}

fn rewrite_supported_correlated_exists_in_join_filter(
    expr: &Expr,
    join_aliases: &[String],
) -> Expr {
    use core::ops::ControlFlow;
    use sqlparser::ast::{VisitMut, VisitorMut};

    fn expr_contains_or(expr: &Expr) -> bool {
        use core::ops::ControlFlow;
        use sqlparser::ast::visit_expressions;

        let mut found = false;
        let _ = visit_expressions(expr, |e| match e {
            Expr::BinaryOp {
                op: BinaryOperator::Or,
                ..
            } => {
                found = true;
                ControlFlow::Break(())
            }
            _ => ControlFlow::Continue(()),
        });
        found
    }

    fn collect_and_conjuncts(expr: &Expr, out: &mut Vec<Expr>) {
        match expr {
            Expr::BinaryOp {
                left,
                op: BinaryOperator::And,
                right,
            } => {
                collect_and_conjuncts(left, out);
                collect_and_conjuncts(right, out);
            }
            Expr::Nested(inner) => collect_and_conjuncts(inner, out),
            other => out.push(other.clone()),
        }
    }

    fn combine_conjuncts(mut conjuncts: Vec<Expr>) -> Option<Expr> {
        if conjuncts.is_empty() {
            return None;
        }
        let first = conjuncts.remove(0);
        Some(
            conjuncts
                .into_iter()
                .fold(first, |acc, next| Expr::BinaryOp {
                    left: Box::new(acc),
                    op: BinaryOperator::And,
                    right: Box::new(next),
                }),
        )
    }

    fn unqualified_table_part(expr: &Expr) -> Option<&sqlparser::ast::Ident> {
        match expr {
            Expr::CompoundIdentifier(parts) if parts.len() >= 2 => parts.get(parts.len() - 2),
            _ => None,
        }
    }

    fn try_extract_outer_col(expr: &Expr, outer_alias: &str) -> Option<Expr> {
        match expr {
            Expr::CompoundIdentifier(parts) if parts.len() >= 2 => {
                let table_part = names::normalize_ident(&parts[parts.len() - 2]);
                table_part
                    .eq_ignore_ascii_case(outer_alias)
                    .then(|| expr.clone())
            }
            _ => None,
        }
    }

    fn try_extract_inner_col(expr: &Expr, inner_exposed_name: &str) -> Option<Expr> {
        match expr {
            Expr::Identifier(_) => Some(expr.clone()),
            Expr::CompoundIdentifier(parts) if parts.len() >= 2 => {
                let table_part = names::normalize_ident(&parts[parts.len() - 2]);
                table_part
                    .eq_ignore_ascii_case(inner_exposed_name)
                    .then(|| expr.clone())
            }
            _ => None,
        }
    }

    fn try_decorrelate_exists_to_in(subquery: &Query, join_aliases: &[String]) -> Option<Expr> {
        // Only support a single correlated outer alias (fail closed for multiple/zero).
        let mut outer_refs: Vec<String> = Vec::new();
        for alias in join_aliases {
            if crate::sql::executor::subquery::query_has_outer_reference(subquery, alias) {
                outer_refs.push(alias.clone());
            }
        }
        if outer_refs.len() != 1 {
            return None;
        }
        let outer_alias = outer_refs.swap_remove(0);

        // Disallow complex query decorations (ORDER BY / LIMIT / OFFSET / WITH / etc).
        if subquery.with.is_some()
            || !subquery.order_by.is_empty()
            || subquery.limit.is_some()
            || !subquery.limit_by.is_empty()
            || subquery.offset.is_some()
            || subquery.fetch.is_some()
            || !subquery.locks.is_empty()
            || subquery.for_clause.is_some()
        {
            return None;
        }

        let select = match subquery.body.as_ref() {
            SetExpr::Select(sel) => sel.as_ref(),
            _ => return None,
        };

        if select.distinct.is_some()
            || select.top.is_some()
            || select.into.is_some()
            || !select.lateral_views.is_empty()
            || select.from.len() != 1
            || !matches!(&select.group_by, GroupByExpr::Expressions(exprs) if exprs.is_empty())
            || !select.cluster_by.is_empty()
            || !select.distribute_by.is_empty()
            || !select.sort_by.is_empty()
            || select.having.is_some()
            || !select.named_window.is_empty()
            || select.qualify.is_some()
        {
            return None;
        }

        let twj = select.from.first()?;
        if !twj.joins.is_empty() {
            return None;
        }

        let inner_exposed_name: String = match &twj.relation {
            TableFactor::Table { name, alias, .. } => alias
                .as_ref()
                .map(|a| a.name.value.clone())
                .unwrap_or_else(|| {
                    names::split_object_name(name)
                        .map(|(_, obj)| obj)
                        .unwrap_or_else(|_| {
                            name.0
                                .last()
                                .map(|ident| ident.value.clone())
                                .unwrap_or_default()
                        })
                }),
            _ => return None,
        };

        // If the inner FROM shadows the outer alias, we can't safely identify correlation via
        // qualified names here.
        if inner_exposed_name.eq_ignore_ascii_case(&outer_alias) {
            return None;
        }

        let where_expr = select.selection.as_ref()?;

        // Keep the supported shape tight: only AND conjunctions (no OR anywhere).
        if expr_contains_or(where_expr) {
            return None;
        }

        let mut conjuncts: Vec<Expr> = Vec::new();
        collect_and_conjuncts(where_expr, &mut conjuncts);
        if conjuncts.is_empty() {
            return None;
        }

        let mut correlated_idx: Option<usize> = None;
        for (idx, conj) in conjuncts.iter().enumerate() {
            if crate::sql::executor::subquery::expr_has_outer_reference(conj, &outer_alias) {
                if correlated_idx.is_some() {
                    // Outer alias appears in more than one conjunct (unsupported).
                    return None;
                }
                correlated_idx = Some(idx);
            }
        }
        let correlated_idx = correlated_idx?;

        // Correlated conjunct must be a simple inner_col = outer_alias.outer_col equality.
        let correlated_conj = match &conjuncts[correlated_idx] {
            Expr::Nested(inner) => inner.as_ref(),
            other => other,
        };
        let (outer_col, inner_col) = match correlated_conj {
            Expr::BinaryOp {
                left,
                op: BinaryOperator::Eq,
                right,
            } => {
                if let Some(outer_col) = try_extract_outer_col(left, &outer_alias) {
                    let inner_col = try_extract_inner_col(right, &inner_exposed_name)?;
                    (outer_col, inner_col)
                } else if let Some(outer_col) = try_extract_outer_col(right, &outer_alias) {
                    let inner_col = try_extract_inner_col(left, &inner_exposed_name)?;
                    (outer_col, inner_col)
                } else {
                    return None;
                }
            }
            _ => return None,
        };

        // Ensure the "inner col" side isn't accidentally another qualified outer ref.
        if let Some(table_part) = unqualified_table_part(&inner_col) {
            let table_part = names::normalize_ident(table_part);
            if table_part.eq_ignore_ascii_case(&outer_alias) {
                return None;
            }
        }

        // Remaining conjuncts must be fully uncorrelated (no join-alias outer refs).
        for (idx, conj) in conjuncts.iter().enumerate() {
            if idx == correlated_idx {
                continue;
            }
            for alias in join_aliases {
                if crate::sql::executor::subquery::expr_has_outer_reference(conj, alias) {
                    return None;
                }
            }
        }

        // Build the uncorrelated IN-subquery: SELECT inner_col FROM <table> WHERE <remaining>.
        let mut remaining: Vec<Expr> = Vec::new();
        for (idx, conj) in conjuncts.into_iter().enumerate() {
            if idx != correlated_idx {
                remaining.push(conj);
            }
        }
        let new_selection = combine_conjuncts(remaining);

        let mut new_select = select.clone();
        new_select.projection = vec![SelectItem::UnnamedExpr(inner_col)];
        new_select.selection = new_selection;

        let new_query = Query {
            with: None,
            body: Box::new(SetExpr::Select(Box::new(new_select))),
            order_by: Vec::new(),
            limit: None,
            limit_by: Vec::new(),
            offset: None,
            fetch: None,
            locks: Vec::new(),
            for_clause: None,
        };

        // Final safety check: rewritten subquery must not contain any outer refs.
        for alias in join_aliases {
            if crate::sql::executor::subquery::query_has_outer_reference(&new_query, alias) {
                return None;
            }
        }

        Some(Expr::InSubquery {
            expr: Box::new(outer_col),
            subquery: Box::new(new_query),
            negated: false,
        })
    }

    #[derive(Clone)]
    struct SupportedExistsRewriter<'a> {
        join_aliases: &'a [String],
        query_depth: usize,
        negation_depth: usize,
    }

    impl<'a> VisitorMut for SupportedExistsRewriter<'a> {
        type Break = ();

        fn pre_visit_query(&mut self, _query: &mut Query) -> ControlFlow<Self::Break> {
            self.query_depth = self.query_depth.saturating_add(1);
            ControlFlow::Continue(())
        }

        fn post_visit_query(&mut self, _query: &mut Query) -> ControlFlow<Self::Break> {
            self.query_depth = self.query_depth.saturating_sub(1);
            ControlFlow::Continue(())
        }

        fn pre_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<Self::Break> {
            // Track when we enter a NOT context to avoid rewriting NOT (EXISTS ...) to NOT (IN ...)
            // since IN can return NULL and get filtered incorrectly.
            if matches!(
                expr,
                Expr::UnaryOp {
                    op: sqlparser::ast::UnaryOperator::Not,
                    ..
                }
            ) {
                self.negation_depth = self.negation_depth.saturating_add(1);
            }
            ControlFlow::Continue(())
        }

        fn post_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<Self::Break> {
            // Only rewrite at the current (outer) query block.
            if self.query_depth > 0 {
                // Track negation depth even in nested queries for correct unwinding
                if matches!(
                    expr,
                    Expr::UnaryOp {
                        op: sqlparser::ast::UnaryOperator::Not,
                        ..
                    }
                ) {
                    self.negation_depth = self.negation_depth.saturating_sub(1);
                }
                return ControlFlow::Continue(());
            }

            if let Expr::Exists { subquery, negated } = expr {
                // Only rewrite EXISTS to IN if:
                // 1. It's not negated in the Exists node itself
                // 2. It's not wrapped in a NOT operator (negation_depth == 0)
                // This prevents rewriting NOT (EXISTS ...) to NOT (IN ...), which has wrong NULL semantics.
                if !*negated && self.negation_depth == 0 {
                    if let Some(rewritten) =
                        try_decorrelate_exists_to_in(subquery.as_ref(), self.join_aliases)
                    {
                        *expr = rewritten;
                    }
                }
            }

            // Unwind negation depth when exiting a NOT operator
            if matches!(
                expr,
                Expr::UnaryOp {
                    op: sqlparser::ast::UnaryOperator::Not,
                    ..
                }
            ) {
                self.negation_depth = self.negation_depth.saturating_sub(1);
            }

            ControlFlow::Continue(())
        }
    }

    let mut out = expr.clone();
    let mut visitor = SupportedExistsRewriter {
        join_aliases,
        query_depth: 0,
        negation_depth: 0,
    };
    let _ = out.visit(&mut visitor);
    out
}

struct TableInfo {
    alias: String,
    is_system_catalog: bool,
    schema: TableSchema,
    preloaded_rows: Option<Vec<Row>>,
}

impl Executor {
    pub(in crate::sql::executor::select) async fn try_execute_simple_join_with_operators(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        query: &Query,
        select: &sqlparser::ast::Select,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Result<Option<ExecuteResult>> {
        use crate::types::ColumnDef;

        let join_aliases: Vec<String> = {
            let mut aliases: Vec<String> = Vec::new();
            for twj in &select.from {
                aliases.extend(collect_visible_aliases_in_table_with_joins(twj));
            }
            let mut seen: HashSet<String> = HashSet::new();
            aliases.retain(|a| seen.insert(a.to_lowercase()));
            aliases
        };

        let mut select = select.clone();
        if let Some(sel) = select.selection.as_mut() {
            *sel = rewrite_supported_correlated_exists_in_join_filter(sel, &join_aliases);
        }
        if let Some(having) = select.having.as_mut() {
            *having = rewrite_supported_correlated_exists_in_join_filter(having, &join_aliases);
        }

        // Fail-closed semantics: JOIN-context correlated subqueries are not supported.
        // If any scalar/EXISTS/IN subquery in SELECT/WHERE/HAVING references any JOIN-visible table
        // alias, fail fast with an explicit Unsupported error.
        //
        // NOTE: JOIN ... ON scalar subqueries are handled later by join-condition rewriting, so we
        // intentionally do not pre-check them here (required for ORM/system-catalog queries).
        {
            use core::ops::ControlFlow;
            use sqlparser::ast::visit_expressions;

            fn collect_correlated_outer_refs_in_expr(
                expr: &Expr,
                join_aliases: &[String],
                out: &mut HashSet<String>,
            ) {
                let _ = visit_expressions(expr, |e| {
                    let q = match e {
                        Expr::Subquery(q) => Some(q.as_ref()),
                        Expr::InSubquery { subquery, .. } => Some(subquery.as_ref()),
                        Expr::Exists { subquery, .. } => Some(subquery.as_ref()),
                        _ => None,
                    };
                    if let Some(q) = q {
                        for alias in join_aliases {
                            if crate::sql::executor::subquery::query_has_outer_reference(q, alias) {
                                out.insert(alias.clone());
                            }
                        }
                    }
                    ControlFlow::<()>::Continue(())
                });
            }

            let mut correlated_outer_refs: HashSet<String> = HashSet::new();

            for item in &select.projection {
                match item {
                    SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                        collect_correlated_outer_refs_in_expr(
                            expr,
                            &join_aliases,
                            &mut correlated_outer_refs,
                        );
                    }
                    _ => {}
                }
            }

            if let Some(sel) = select.selection.as_ref() {
                collect_correlated_outer_refs_in_expr(
                    sel,
                    &join_aliases,
                    &mut correlated_outer_refs,
                );
            }

            if let Some(having) = select.having.as_ref() {
                collect_correlated_outer_refs_in_expr(
                    having,
                    &join_aliases,
                    &mut correlated_outer_refs,
                );
            }

            if !correlated_outer_refs.is_empty() {
                let outer_refs: Vec<String> = correlated_outer_refs.into_iter().collect();
                return Err(correlated_subquery_join_context_unsupported(outer_refs));
            }
        }

        let virtual_filter = select
            .selection
            .as_ref()
            .map(extract_virtual_table_filter)
            .unwrap_or_default();

        let resolved_selection = if let Some(sel) = &select.selection {
            Some(
                self.resolve_subqueries(txn, db_id, sequence_values, search_path, sel, ctes, &[])
                    .await?,
            )
        } else {
            None
        };

        let resolved_projection = self
            .resolve_projection_subqueries(
                txn,
                db_id,
                sequence_values,
                search_path,
                &select.projection,
                ctes,
            )
            .await?;

        let has_lateral = select.from.iter().any(|from_item| {
            from_item
                .joins
                .iter()
                .any(|j| matches!(&j.relation, TableFactor::Derived { lateral: true, .. }))
        });

        if has_lateral {
            return self
                .execute_lateral_join(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    query,
                    &select,
                    &resolved_projection,
                    resolved_selection.as_ref(),
                    ctes,
                )
                .await
                .map(Some);
        }

        let is_implicit_join = select.from.len() > 1;

        fn table_factor_is_system_catalog(factor: &TableFactor) -> bool {
            let schema_opt = match factor {
                TableFactor::Table { name, .. } => match names::split_object_name(name) {
                    Ok((schema_opt, _)) => schema_opt,
                    Err(_) => return false,
                },
                _ => return false,
            };

            schema_opt.is_some_and(|schema| {
                schema.eq_ignore_ascii_case("information_schema")
                    || schema.eq_ignore_ascii_case("pg_catalog")
                    || schema.eq_ignore_ascii_case("pg_toast")
            })
        }

        let mut tables: Vec<TableInfo> = Vec::new();
        // `build_join_wildcard_plan()` expects sources in `select.from` flattened order, which can
        // differ from the physical `tables` order we build for correct JOIN binding precedence.
        // Track the resolved alias for each flattened source so we can later remap wildcard plans
        // back onto the executor's `tables` layout.
        let mut from_source_aliases: Vec<Vec<String>> = vec![Vec::new(); select.from.len()];
        struct JoinStep {
            right_idx: usize,
            join_type: JoinType,
            condition: Option<Expr>,
        }
        let mut join_steps: Vec<JoinStep> = Vec::new();
        let mut merge_columns: Vec<UsingMergeColumn> = Vec::new();

        // Unified path: process all FROM items and their explicit JOINs.
        // For `FROM a, b JOIN c ON ...`, sqlparser gives:
        //   from[0] = {relation: a, joins: []}
        //   from[1] = {relation: b, joins: [{relation: c, ON: b.id = c.id}]}
        //
        // Explicit JOINs bind tighter than comma: `FROM a, b RIGHT JOIN c`
        // means `a CROSS (b RIGHT JOIN c)`. We process FROM items with explicit
        // JOINs first, then cross-join the standalone FROM items.
        // Phase 1: process FROM items that have explicit JOINs (they bind tighter)
        let mut transparent_nested_joins: Vec<TransparentNestedJoinInfo> = Vec::new();
        for (from_idx, from_item) in select.from.iter().enumerate() {
            if from_item.joins.is_empty() {
                continue;
            }
            let resolved_table = self
                .resolve_join_table_factor(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    &from_item.relation,
                    ctes,
                    &virtual_filter,
                )
                .await?;
            let (alias, schema, preloaded_rows) = match resolved_table {
                Some(t) => t,
                None => return Ok(None),
            };
            let is_system_catalog = table_factor_is_system_catalog(&from_item.relation);
            from_source_aliases[from_idx].push(alias.clone());
            if let TableFactor::NestedJoin {
                table_with_joins,
                alias: nested_alias,
            } = &from_item.relation
            {
                if nested_alias.is_none() {
                    let inner_aliases =
                        collect_visible_aliases_in_table_with_joins(table_with_joins);
                    if !inner_aliases.is_empty() {
                        transparent_nested_joins.push(TransparentNestedJoinInfo {
                            derived_alias: alias.clone(),
                            inner_aliases,
                            duplicate_cols_lower: duplicate_column_names_lowercase(&schema),
                        });
                    }
                }
            }
            tables.push(TableInfo {
                alias,
                is_system_catalog,
                schema,
                preloaded_rows,
            });

            for join in &from_item.joins {
                let resolved_join = self
                    .resolve_join_table_factor(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        &join.relation,
                        ctes,
                        &virtual_filter,
                    )
                    .await?;
                let (alias, right_schema, right_preloaded) = match resolved_join {
                    Some(t) => t,
                    None => return Ok(None),
                };
                let is_system_catalog = table_factor_is_system_catalog(&join.relation);
                from_source_aliases[from_idx].push(alias.clone());
                if let TableFactor::NestedJoin {
                    table_with_joins,
                    alias: nested_alias,
                } = &join.relation
                {
                    if nested_alias.is_none() {
                        let inner_aliases =
                            collect_visible_aliases_in_table_with_joins(table_with_joins);
                        if !inner_aliases.is_empty() {
                            transparent_nested_joins.push(TransparentNestedJoinInfo {
                                derived_alias: alias.clone(),
                                inner_aliases,
                                duplicate_cols_lower: duplicate_column_names_lowercase(
                                    &right_schema,
                                ),
                            });
                        }
                    }
                }
                let jt = JoinType::from(&join.join_operator);
                let condition = match &join.join_operator {
                    sqlparser::ast::JoinOperator::Inner(JoinConstraint::On(expr))
                    | sqlparser::ast::JoinOperator::LeftOuter(JoinConstraint::On(expr))
                    | sqlparser::ast::JoinOperator::RightOuter(JoinConstraint::On(expr))
                    | sqlparser::ast::JoinOperator::FullOuter(JoinConstraint::On(expr)) => {
                        Some(expr.clone())
                    }
                    sqlparser::ast::JoinOperator::CrossJoin => None,
                    sqlparser::ast::JoinOperator::Inner(JoinConstraint::Using(cols))
                    | sqlparser::ast::JoinOperator::LeftOuter(JoinConstraint::Using(cols))
                    | sqlparser::ast::JoinOperator::RightOuter(JoinConstraint::Using(cols))
                    | sqlparser::ast::JoinOperator::FullOuter(JoinConstraint::Using(cols)) => {
                        let mut conditions: Vec<Expr> = Vec::new();
                        for col in cols {
                            let col_name = names::normalize_ident(col);
                            let left_expr = if let Some(idx) = merge_columns
                                .iter()
                                .position(|mc| mc.col_name.eq_ignore_ascii_case(&col_name))
                            {
                                // Chained USING JOIN must compare against the *merged* key from
                                // the left relation (COALESCE semantics), not any single prior
                                // table alias. Important for OUTER JOIN chains.
                                let expr = build_coalesce_for_merge(&merge_columns[idx]);
                                merge_columns[idx].source_aliases.push(alias.clone());
                                expr
                            } else {
                                let left_alias = tables
                                    .iter()
                                    .rev()
                                    .find(|t| {
                                        t.schema
                                            .columns
                                            .iter()
                                            .any(|c| c.name.eq_ignore_ascii_case(&col_name))
                                    })
                                    .map(|t| t.alias.clone());
                                let left_alias = match left_alias {
                                    Some(a) => a,
                                    None => return Ok(None),
                                };
                                merge_columns.push(UsingMergeColumn {
                                    col_name: col_name.clone(),
                                    source_aliases: vec![left_alias.clone(), alias.clone()],
                                });
                                Expr::CompoundIdentifier(vec![
                                    Ident::new(left_alias),
                                    Ident::new(col_name.clone()),
                                ])
                            };
                            conditions.push(Expr::BinaryOp {
                                left: Box::new(left_expr),
                                op: BinaryOperator::Eq,
                                right: Box::new(Expr::CompoundIdentifier(vec![
                                    Ident::new(alias.clone()),
                                    Ident::new(col_name),
                                ])),
                            });
                        }
                        conditions.into_iter().reduce(|a, b| Expr::BinaryOp {
                            left: Box::new(a),
                            op: BinaryOperator::And,
                            right: Box::new(b),
                        })
                    }
                    sqlparser::ast::JoinOperator::Inner(JoinConstraint::Natural)
                    | sqlparser::ast::JoinOperator::LeftOuter(JoinConstraint::Natural)
                    | sqlparser::ast::JoinOperator::RightOuter(JoinConstraint::Natural)
                    | sqlparser::ast::JoinOperator::FullOuter(JoinConstraint::Natural) => {
                        let mut common_idents: Vec<Ident> = Vec::new();
                        let mut seen = HashSet::new();
                        for right_col in &right_schema.columns {
                            let col_lower = right_col.name.to_lowercase();
                            if seen.contains(&col_lower) {
                                continue;
                            }
                            for left_table in &tables {
                                if left_table
                                    .schema
                                    .columns
                                    .iter()
                                    .any(|c| c.name.eq_ignore_ascii_case(&col_lower))
                                {
                                    common_idents.push(Ident::new(col_lower.clone()));
                                    seen.insert(col_lower.clone());
                                    break;
                                }
                            }
                        }
                        if common_idents.is_empty() {
                            None
                        } else {
                            let mut conditions: Vec<Expr> = Vec::new();
                            for col_ident in &common_idents {
                                let col_name = &col_ident.value;
                                let left_expr = if let Some(idx) = merge_columns
                                    .iter()
                                    .position(|mc| mc.col_name.eq_ignore_ascii_case(col_name))
                                {
                                    let expr = build_coalesce_for_merge(&merge_columns[idx]);
                                    merge_columns[idx].source_aliases.push(alias.clone());
                                    expr
                                } else {
                                    let left_alias = tables
                                        .iter()
                                        .rev()
                                        .find(|t| {
                                            t.schema
                                                .columns
                                                .iter()
                                                .any(|c| c.name.eq_ignore_ascii_case(col_name))
                                        })
                                        .map(|t| t.alias.clone())
                                        .unwrap_or_else(|| tables[0].alias.clone());
                                    merge_columns.push(UsingMergeColumn {
                                        col_name: col_name.clone(),
                                        source_aliases: vec![left_alias.clone(), alias.clone()],
                                    });
                                    Expr::CompoundIdentifier(vec![
                                        Ident::new(left_alias),
                                        Ident::new(col_name.clone()),
                                    ])
                                };
                                conditions.push(Expr::BinaryOp {
                                    left: Box::new(left_expr),
                                    op: BinaryOperator::Eq,
                                    right: Box::new(Expr::CompoundIdentifier(vec![
                                        Ident::new(alias.clone()),
                                        Ident::new(col_name.clone()),
                                    ])),
                                });
                            }
                            conditions.into_iter().reduce(|a, b| Expr::BinaryOp {
                                left: Box::new(a),
                                op: BinaryOperator::And,
                                right: Box::new(b),
                            })
                        }
                    }
                    _ => return Ok(None),
                };
                let idx = tables.len();
                tables.push(TableInfo {
                    alias,
                    is_system_catalog,
                    schema: right_schema,
                    preloaded_rows: right_preloaded,
                });
                join_steps.push(JoinStep {
                    right_idx: idx,
                    join_type: jt,
                    condition,
                });
            }
        }

        // Phase 2: add standalone FROM items (no explicit JOINs) as CROSS JOINs
        for (from_idx, from_item) in select.from.iter().enumerate() {
            if !from_item.joins.is_empty() {
                continue;
            }
            let resolved_table = self
                .resolve_join_table_factor(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    &from_item.relation,
                    ctes,
                    &virtual_filter,
                )
                .await?;
            let (alias, schema, preloaded_rows) = match resolved_table {
                Some(t) => t,
                None => return Ok(None),
            };
            let is_system_catalog = table_factor_is_system_catalog(&from_item.relation);
            from_source_aliases[from_idx].push(alias.clone());
            let idx = tables.len();
            let need_cross = idx > 0;
            if let TableFactor::NestedJoin {
                table_with_joins,
                alias: nested_alias,
            } = &from_item.relation
            {
                if nested_alias.is_none() {
                    let inner_aliases =
                        collect_visible_aliases_in_table_with_joins(table_with_joins);
                    if !inner_aliases.is_empty() {
                        transparent_nested_joins.push(TransparentNestedJoinInfo {
                            derived_alias: alias.clone(),
                            inner_aliases,
                            duplicate_cols_lower: duplicate_column_names_lowercase(&schema),
                        });
                    }
                }
            }
            tables.push(TableInfo {
                alias,
                is_system_catalog,
                schema,
                preloaded_rows,
            });
            if need_cross {
                join_steps.push(JoinStep {
                    right_idx: idx,
                    join_type: JoinType::Cross,
                    condition: None,
                });
            }
        }

        if tables.len() < 2 {
            return Ok(None);
        }

        let mut correlated_subquery_counter: usize = 0;

        fn rewrite_join_condition_subqueries<'a>(
            exec: &'a Executor,
            txn: &'a mut Transaction,
            db_id: u64,
            sequence_values: &'a mut HashMap<String, i64>,
            search_path: &'a [String],
            expr: &'a Expr,
            ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
            tables: &'a mut Vec<TableInfo>,
            counter: &'a mut usize,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Expr>> + Send + 'a>>
        {
            Box::pin(async move {
                match expr {
                    Expr::Subquery(subquery) => {
                        let mut referenced_aliases: Vec<String> = Vec::new();
                        for t in tables.iter() {
                            if crate::sql::executor::subquery::query_has_outer_reference(
                                subquery, &t.alias,
                            ) {
                                referenced_aliases.push(t.alias.clone());
                            }
                        }

                        if referenced_aliases.is_empty() {
                            return exec
                                .resolve_subqueries(
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    expr,
                                    ctes,
                                    &[],
                                )
                                .await;
                        }

                        if referenced_aliases.len() != 1 {
                            return Err(correlated_subquery_join_context_unsupported(
                                referenced_aliases,
                            ));
                        }

                        let outer_alias = referenced_aliases
                            .first()
                            .cloned()
                            .unwrap_or_else(|| "outer".to_string());

                        let table_idx = tables
                            .iter()
                            .position(|t| t.alias.eq_ignore_ascii_case(&outer_alias))
                            .ok_or_else(|| {
                                anyhow!(
                                    "Correlated scalar subquery references unknown outer table alias '{}'",
                                    outer_alias
                                )
                            })?;

                        if !tables[table_idx].is_system_catalog {
                            return Err(correlated_subquery_join_context_unsupported(vec![
                                outer_alias,
                            ]));
                        }

                        if tables[table_idx].preloaded_rows.is_none() {
                            let (schema, rows) = exec
                                .get_table_data(
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    &tables[table_idx].schema.name,
                                    ctes,
                                )
                                .await?;
                            tables[table_idx].schema = schema;
                            tables[table_idx].preloaded_rows = Some(rows);
                        }

                        let computed_col = format!("__tipg_subquery_{}", *counter);
                        *counter = counter.saturating_add(1);

                        let outer_schema = tables[table_idx].schema.clone();
                        let outer_rows =
                            tables[table_idx].preloaded_rows.take().unwrap_or_default();

                        let mut inferred_type: Option<DataType> = None;
                        let mut new_rows: Vec<Row> = Vec::with_capacity(outer_rows.len());
                        for row in outer_rows {
                            let val = exec
                                .eval_correlated_subquery(
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    subquery,
                                    &outer_alias,
                                    &outer_schema,
                                    &row,
                                    ctes,
                                )
                                .await?;
                            if inferred_type.is_none() {
                                inferred_type = val.data_type();
                            }
                            let mut values = row.values;
                            values.push(val);
                            new_rows.push(Row::new(values));
                        }

                        tables[table_idx]
                            .schema
                            .columns
                            .push(crate::types::ColumnDef {
                                name: computed_col.clone(),
                                // INTENTIONAL: all-NULL subquery column defaults to Text (PG-compatible)
                                data_type: inferred_type.unwrap_or(DataType::Text),
                                nullable: true,
                                primary_key: false,
                                unique: false,
                                is_serial: false,
                                default_expr: None,
                            });
                        tables[table_idx].preloaded_rows = Some(new_rows);

                        Ok(Expr::CompoundIdentifier(vec![
                            Ident::new(outer_alias),
                            Ident::new(computed_col),
                        ]))
                    }
                    Expr::Exists { subquery, .. } => {
                        let mut referenced_aliases: Vec<String> = Vec::new();
                        for t in tables.iter() {
                            if crate::sql::executor::subquery::query_has_outer_reference(
                                subquery, &t.alias,
                            ) {
                                referenced_aliases.push(t.alias.clone());
                            }
                        }
                        if referenced_aliases.is_empty() {
                            Ok(expr.clone())
                        } else {
                            Err(correlated_subquery_join_context_unsupported(
                                referenced_aliases,
                            ))
                        }
                    }
                    Expr::InSubquery { subquery, .. } => {
                        let mut referenced_aliases: Vec<String> = Vec::new();
                        for t in tables.iter() {
                            if crate::sql::executor::subquery::query_has_outer_reference(
                                subquery, &t.alias,
                            ) {
                                referenced_aliases.push(t.alias.clone());
                            }
                        }
                        if referenced_aliases.is_empty() {
                            Ok(expr.clone())
                        } else {
                            Err(correlated_subquery_join_context_unsupported(
                                referenced_aliases,
                            ))
                        }
                    }
                    Expr::BinaryOp { left, op, right } => {
                        let left = rewrite_join_condition_subqueries(
                            exec,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            left,
                            ctes,
                            tables,
                            counter,
                        )
                        .await?;
                        let right = rewrite_join_condition_subqueries(
                            exec,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            right,
                            ctes,
                            tables,
                            counter,
                        )
                        .await?;
                        Ok(Expr::BinaryOp {
                            left: Box::new(left),
                            op: op.clone(),
                            right: Box::new(right),
                        })
                    }
                    Expr::UnaryOp { op, expr: inner } => {
                        let inner = rewrite_join_condition_subqueries(
                            exec,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            inner,
                            ctes,
                            tables,
                            counter,
                        )
                        .await?;
                        Ok(Expr::UnaryOp {
                            op: op.clone(),
                            expr: Box::new(inner),
                        })
                    }
                    Expr::Nested(inner) => {
                        let inner = rewrite_join_condition_subqueries(
                            exec,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            inner,
                            ctes,
                            tables,
                            counter,
                        )
                        .await?;
                        Ok(Expr::Nested(Box::new(inner)))
                    }
                    Expr::Function(f) => {
                        let mut args = Vec::with_capacity(f.args.len());
                        for arg in &f.args {
                            let rewritten = match arg {
                                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                                    FunctionArg::Unnamed(FunctionArgExpr::Expr(
                                        rewrite_join_condition_subqueries(
                                            exec,
                                            txn,
                                            db_id,
                                            sequence_values,
                                            search_path,
                                            e,
                                            ctes,
                                            tables,
                                            counter,
                                        )
                                        .await?,
                                    ))
                                }
                                other => other.clone(),
                            };
                            args.push(rewritten);
                        }
                        Ok(Expr::Function(Function {
                            name: f.name.clone(),
                            args,
                            filter: f.filter.clone(),
                            null_treatment: f.null_treatment.clone(),
                            over: f.over.clone(),
                            distinct: f.distinct,
                            special: f.special,
                            order_by: f.order_by.clone(),
                        }))
                    }
                    _ => Ok(expr.clone()),
                }
            })
        }

        for step in &mut join_steps {
            if let Some(cond) = &step.condition {
                step.condition = Some(
                    rewrite_join_condition_subqueries(
                        self,
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        cond,
                        ctes,
                        &mut tables,
                        &mut correlated_subquery_counter,
                    )
                    .await?,
                );
            }
        }

        let has_aggregates = projection_has_non_window_aggregate(&resolved_projection);
        let has_group_by = !matches!(
            &select.group_by,
            GroupByExpr::Expressions(exprs) if exprs.is_empty()
        );
        let needs_aggregation = has_aggregates || has_group_by || select.having.is_some();

        let has_distinct = select.distinct.is_some();

        // Build (alias, schema) pairs for expression rewriting, with optional alias-less NestedJoin
        // transparency mapping (Sequelize relies on inner aliases remaining visible).
        let mut table_aliases: Vec<(String, TableSchema)> = tables
            .iter()
            .map(|t| (t.alias.clone(), t.schema.clone()))
            .collect();

        let mut requires_transparent_nested_join_mapping = false;
        if !transparent_nested_joins.is_empty() {
            #[derive(Debug, Clone)]
            struct InnerAliasTarget {
                inner_alias: String,
                derived_alias: String,
                duplicate_cols_lower: HashSet<String>,
            }

            let mut inner_alias_targets: HashMap<String, InnerAliasTarget> = HashMap::new();
            let mut ambiguous_inner_aliases: HashSet<String> = HashSet::new();
            for info in &transparent_nested_joins {
                for inner_alias in &info.inner_aliases {
                    let key = inner_alias.to_lowercase();
                    if let Some(existing) = inner_alias_targets.get(&key) {
                        if !existing
                            .derived_alias
                            .eq_ignore_ascii_case(&info.derived_alias)
                        {
                            ambiguous_inner_aliases.insert(key);
                        }
                    } else {
                        inner_alias_targets.insert(
                            key,
                            InnerAliasTarget {
                                inner_alias: inner_alias.clone(),
                                derived_alias: info.derived_alias.clone(),
                                duplicate_cols_lower: info.duplicate_cols_lower.clone(),
                            },
                        );
                    }
                }
            }

            // Guard-rail: Qualified wildcard (inner_alias.*) is not transparent today because it is
            // expanded from `tables` rather than `table_aliases`.
            for item in &resolved_projection {
                if let SelectItem::QualifiedWildcard(obj, _) = item {
                    let qualifier = obj.0.last().map(|i| i.value.clone()).unwrap_or_default();
                    if inner_alias_targets.contains_key(&qualifier.to_lowercase()) {
                        return Err(SqlError::Unsupported(format!(
                            "Qualified wildcard {}.* over nested join grouping is not supported",
                            qualifier
                        ))
                        .into());
                    }
                }
            }

            let mut used_inner_aliases: HashSet<String> = HashSet::new();

            fn visit_expr_for_inner_alias_refs(
                expr: &Expr,
                targets: &HashMap<String, InnerAliasTarget>,
                ambiguous: &HashSet<String>,
                used: &mut HashSet<String>,
            ) -> Result<()> {
                match expr {
                    Expr::CompoundIdentifier(parts) if parts.len() == 2 => {
                        let table_ref = parts[0].value.to_lowercase();
                        if targets.contains_key(&table_ref) {
                            if ambiguous.contains(&table_ref) {
                                return Err(SqlError::Unsupported(format!(
                                    "Ambiguous nested join inner alias {}",
                                    parts[0].value
                                ))
                                .into());
                            }
                            used.insert(table_ref.clone());

                            let Some(target) = targets.get(&table_ref) else {
                                return Ok(());
                            };
                            let col_lower = parts[1].value.to_lowercase();
                            if target.duplicate_cols_lower.contains(&col_lower) {
                                return Err(SqlError::Unsupported(format!(
                                    "NestedJoin materialization cannot safely resolve {}.{} because derived output contains duplicate column name {}",
                                    parts[0].value,
                                    parts[1].value,
                                    parts[1].value
                                ))
                                .into());
                            }
                        }
                        Ok(())
                    }
                    Expr::BinaryOp { left, right, .. } => {
                        visit_expr_for_inner_alias_refs(left, targets, ambiguous, used)?;
                        visit_expr_for_inner_alias_refs(right, targets, ambiguous, used)
                    }
                    Expr::UnaryOp { expr: inner, .. }
                    | Expr::Nested(inner)
                    | Expr::IsNull(inner)
                    | Expr::IsNotNull(inner) => {
                        visit_expr_for_inner_alias_refs(inner, targets, ambiguous, used)
                    }
                    Expr::Function(f) => {
                        for arg in &f.args {
                            if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = arg {
                                visit_expr_for_inner_alias_refs(e, targets, ambiguous, used)?;
                            }
                        }
                        Ok(())
                    }
                    Expr::Cast { expr: inner, .. } | Expr::TryCast { expr: inner, .. } => {
                        visit_expr_for_inner_alias_refs(inner, targets, ambiguous, used)
                    }
                    Expr::InList {
                        expr: inner, list, ..
                    } => {
                        visit_expr_for_inner_alias_refs(inner, targets, ambiguous, used)?;
                        for item in list {
                            visit_expr_for_inner_alias_refs(item, targets, ambiguous, used)?;
                        }
                        Ok(())
                    }
                    Expr::Between {
                        expr: inner,
                        low,
                        high,
                        ..
                    } => {
                        visit_expr_for_inner_alias_refs(inner, targets, ambiguous, used)?;
                        visit_expr_for_inner_alias_refs(low, targets, ambiguous, used)?;
                        visit_expr_for_inner_alias_refs(high, targets, ambiguous, used)
                    }
                    Expr::Case {
                        operand,
                        conditions,
                        results,
                        else_result,
                    } => {
                        if let Some(op) = operand.as_deref() {
                            visit_expr_for_inner_alias_refs(op, targets, ambiguous, used)?;
                        }
                        for c in conditions {
                            visit_expr_for_inner_alias_refs(c, targets, ambiguous, used)?;
                        }
                        for r in results {
                            visit_expr_for_inner_alias_refs(r, targets, ambiguous, used)?;
                        }
                        if let Some(e) = else_result.as_deref() {
                            visit_expr_for_inner_alias_refs(e, targets, ambiguous, used)?;
                        }
                        Ok(())
                    }
                    Expr::Subquery(_) | Expr::Exists { .. } => Ok(()),
                    _ => Ok(()),
                }
            }

            for step in &join_steps {
                if let Some(cond) = &step.condition {
                    visit_expr_for_inner_alias_refs(
                        cond,
                        &inner_alias_targets,
                        &ambiguous_inner_aliases,
                        &mut used_inner_aliases,
                    )?;
                }
            }
            if let Some(sel) = resolved_selection.as_ref() {
                visit_expr_for_inner_alias_refs(
                    sel,
                    &inner_alias_targets,
                    &ambiguous_inner_aliases,
                    &mut used_inner_aliases,
                )?;
            }
            for item in &resolved_projection {
                match item {
                    SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                        visit_expr_for_inner_alias_refs(
                            expr,
                            &inner_alias_targets,
                            &ambiguous_inner_aliases,
                            &mut used_inner_aliases,
                        )?;
                    }
                    _ => {}
                }
            }
            for o in &query.order_by {
                visit_expr_for_inner_alias_refs(
                    &o.expr,
                    &inner_alias_targets,
                    &ambiguous_inner_aliases,
                    &mut used_inner_aliases,
                )?;
            }
            if let GroupByExpr::Expressions(exprs) = &select.group_by {
                for e in exprs {
                    visit_expr_for_inner_alias_refs(
                        e,
                        &inner_alias_targets,
                        &ambiguous_inner_aliases,
                        &mut used_inner_aliases,
                    )?;
                }
            }
            if let Some(having) = select.having.as_ref() {
                visit_expr_for_inner_alias_refs(
                    having,
                    &inner_alias_targets,
                    &ambiguous_inner_aliases,
                    &mut used_inner_aliases,
                )?;
            }
            if let Some(qualify) = select.qualify.as_ref() {
                visit_expr_for_inner_alias_refs(
                    qualify,
                    &inner_alias_targets,
                    &ambiguous_inner_aliases,
                    &mut used_inner_aliases,
                )?;
            }

            for inner_key in &used_inner_aliases {
                if let Some(target) = inner_alias_targets.get(inner_key) {
                    table_aliases.push((
                        target.derived_alias.clone(),
                        TableSchema {
                            name: target.inner_alias.clone(),
                            table_id: 0,
                            columns: Vec::new(),
                            version: 1,
                            pk_constraint_name: None,
                            pk_indices: Vec::new(),
                            indexes: Vec::new(),
                            check_constraints: Vec::new(),
                            foreign_keys: Vec::new(),
                            owner: String::new(),
                            from_alias: None,
                        },
                    ));
                }
            }
            requires_transparent_nested_join_mapping = !used_inner_aliases.is_empty();
        }

        if tables.len() == 2
            && !is_implicit_join
            && !needs_aggregation
            && !has_distinct
            && !requires_transparent_nested_join_mapping
            && merge_columns.is_empty()
            && tables[0].preloaded_rows.is_none()
            && tables[1].preloaded_rows.is_none()
        {
            let limit = extract_limit(query);
            let offset = extract_offset(query);
            let left_preloaded = tables[0].preloaded_rows.take();
            let right_preloaded = tables[1].preloaded_rows.take();
            let result = self
                .execute_simple_join_with_operators(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    tables[0].schema.clone(),
                    tables[1].schema.clone(),
                    &tables[0].alias,
                    &tables[1].alias,
                    join_steps[0].join_type,
                    join_steps[0].condition.clone(),
                    resolved_selection.as_ref(),
                    &query.order_by,
                    limit,
                    offset,
                    &resolved_projection,
                    left_preloaded,
                    right_preloaded,
                )
                .await?;
            return Ok(Some(result));
        }

        // Multi-JOIN: build a left-deep operator tree
        // Build combined schema incrementally
        let mut combined_columns: Vec<ColumnDef> = Vec::new();
        for col in &tables[0].schema.columns {
            combined_columns.push(ColumnDef {
                name: format!("{}.{}", tables[0].alias, col.name),
                data_type: col.data_type.clone(),
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            });
        }

        let mut running_op: BoxedOperator = if let Some(rows) = tables[0].preloaded_rows.take() {
            Box::new(TableScanOperator::new_with_rows(
                tables[0].schema.clone(),
                rows,
            ))
        } else {
            Box::new(TableScanOperator::new(tables[0].schema.clone()))
        };

        for step in &join_steps {
            let right_preloaded_rows = tables[step.right_idx].preloaded_rows.take();
            let right = &tables[step.right_idx];
            for col in &right.schema.columns {
                combined_columns.push(ColumnDef {
                    name: format!("{}.{}", right.alias, col.name),
                    data_type: col.data_type.clone(),
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                });
            }

            let combined_schema = TableSchema {
                name: "join_result".to_string(),
                table_id: 0,
                columns: combined_columns.clone(),
                version: 1,
                pk_constraint_name: None,
                pk_indices: vec![],
                indexes: vec![],
                check_constraints: vec![],
                foreign_keys: vec![],
                owner: String::new(),
                from_alias: None,
            };

            let right_op: BoxedOperator = if let Some(rows) = right_preloaded_rows {
                Box::new(TableScanOperator::new_with_rows(right.schema.clone(), rows))
            } else {
                Box::new(TableScanOperator::new(right.schema.clone()))
            };

            let rewritten_condition = match &step.condition {
                Some(cond) => Some(rewrite_expr_for_multi_join(cond, &table_aliases)?),
                None => None,
            };

            // Try hash join for equi-join conditions.
            // IMPORTANT: Use the rewritten condition (with qualified column names) for join
            // algorithm selection, not the original condition. After the first join, the left
            // schema has qualified names like "a.id", "b.id", so we must use the rewritten
            // condition that references these qualified names.
            let join_algo = choose_join_algorithm(
                rewritten_condition.as_ref(),
                running_op.schema(),
                &right.schema,
                1000,
                1000,
                &HashJoinConfig::default(),
            );

            running_op = match join_algo {
                JoinAlgorithmChoice::HashJoin {
                    left_is_build,
                    left_key_indices,
                    right_key_indices,
                } => {
                    // Split the rewritten condition into equi-join keys and residual filter.
                    // The residual filter contains non-equi predicates that must be applied
                    // after the hash join (e.g., a.val > 10 AND a.id = b.id => residual: a.val > 10).
                    let residual_filter = if let Some(ref cond) = rewritten_condition {
                        use crate::sql::planner::split_join_condition;
                        split_join_condition(cond, running_op.schema(), &right.schema)
                            .and_then(|(_, _, residual)| residual)
                    } else {
                        None
                    };

                    let hash_join_type = match step.join_type {
                        JoinType::Inner => HashJoinType::Inner,
                        JoinType::Left => HashJoinType::Left,
                        JoinType::Right => HashJoinType::Right,
                        JoinType::Full => HashJoinType::Full,
                        JoinType::Cross => HashJoinType::Inner,
                    };
                    Box::new(
                        HashJoinOperator::new(
                            running_op,
                            right_op,
                            hash_join_type,
                            left_key_indices,
                            right_key_indices,
                            left_is_build,
                            residual_filter,
                            HashJoinConfig::default(),
                        )
                        .with_output_schema(combined_schema),
                    )
                }
                JoinAlgorithmChoice::NestedLoop => Box::new(NestedLoopJoinOperator::with_schema(
                    running_op,
                    right_op,
                    step.join_type,
                    rewritten_condition,
                    combined_schema,
                )),
            };
        }

        if let Some(filter) = &resolved_selection {
            let rewritten = rewrite_for_using_join(filter, &table_aliases, &merge_columns)?;
            running_op = Box::new(FilterOperator::new(running_op, rewritten));
        }

        if needs_aggregation {
            return self
                .execute_join_aggregate_path(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    running_op,
                    query,
                    &select,
                    &resolved_projection,
                    &table_aliases,
                    &merge_columns,
                )
                .await;
        }

        let has_window_funcs = projection_has_window_function(&resolved_projection);

        if has_window_funcs {
            return self
                .execute_join_window_path(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    running_op,
                    query,
                    &resolved_projection,
                    &table_aliases,
                    &merge_columns,
                )
                .await;
        }

        let final_schema = running_op.schema().clone();
        let mut rewritten_order_by: Vec<sqlparser::ast::OrderByExpr> = Vec::new();

        let wildcard_plan = if !merge_columns.is_empty() {
            let flattened_aliases: Vec<&str> = from_source_aliases
                .iter()
                .flatten()
                .map(|a| a.as_str())
                .collect();

            let mut flattened_to_table_idx: Vec<usize> =
                Vec::with_capacity(flattened_aliases.len());
            let mut source_schemas: Vec<&TableSchema> = Vec::with_capacity(flattened_aliases.len());
            for alias in &flattened_aliases {
                let table_idx = tables
                    .iter()
                    .position(|t| t.alias.eq_ignore_ascii_case(alias))
                    .ok_or_else(|| {
                        anyhow!("internal error: wildcard source {} not found", alias)
                    })?;
                flattened_to_table_idx.push(table_idx);
                source_schemas.push(&tables[table_idx].schema);
            }

            let mut plan = build_join_wildcard_plan(&select, &source_schemas);
            if let Some(ref mut plan) = plan {
                for col in &mut plan.columns {
                    col.source_idx = flattened_to_table_idx
                        .get(col.source_idx)
                        .copied()
                        .unwrap_or(col.source_idx);
                }
            }
            plan
        } else {
            None
        };

        let source_offsets: Vec<usize> = {
            let mut offsets = Vec::with_capacity(tables.len());
            let mut offset = 0;
            for t in &tables {
                offsets.push(offset);
                offset += t.schema.columns.len();
            }
            offsets
        };

        let mut projection_exprs: Vec<Expr> = Vec::new();
        let mut alias_exprs: HashMap<String, Expr> = HashMap::new();
        for item in &resolved_projection {
            match item {
                SelectItem::Wildcard(_) => {
                    if let Some(ref plan) = wildcard_plan {
                        for wc in &plan.columns {
                            let mc = merge_columns
                                .iter()
                                .find(|mc| mc.col_name.eq_ignore_ascii_case(&wc.name));
                            let is_merge_source = mc.as_ref().map_or(false, |mc| {
                                let wc_alias = tables
                                    .get(wc.source_idx)
                                    .map(|t| t.alias.as_str())
                                    .unwrap_or("");
                                mc.source_aliases
                                    .iter()
                                    .any(|a| a.eq_ignore_ascii_case(wc_alias))
                            });
                            if let Some(mc) = mc.filter(|_| is_merge_source) {
                                projection_exprs.push(build_coalesce_for_merge(mc));
                            } else {
                                let combined_idx = source_offsets[wc.source_idx] + wc.col_idx;
                                projection_exprs.push(Expr::Identifier(Ident::new(
                                    final_schema.columns[combined_idx].name.clone(),
                                )));
                            }
                        }
                    } else {
                        for col in &final_schema.columns {
                            projection_exprs.push(Expr::Identifier(Ident::new(col.name.clone())));
                        }
                    }
                }
                SelectItem::QualifiedWildcard(obj, _) => {
                    let qualifier = obj.0.last().map(|i| i.value.clone()).unwrap_or_default();
                    let (table_idx, table) = tables
                        .iter()
                        .enumerate()
                        .find(|(_, t)| t.alias.eq_ignore_ascii_case(&qualifier))
                        .ok_or_else(|| {
                            anyhow!(
                                "Qualified wildcard {}.* not found in join output",
                                qualifier
                            )
                        })?;

                    for (col_idx, col) in table.schema.columns.iter().enumerate() {
                        if col.name.starts_with("__tipg_subquery_") {
                            continue;
                        }
                        let mc = merge_columns
                            .iter()
                            .find(|mc| mc.col_name.eq_ignore_ascii_case(&col.name));
                        let is_merge_source = mc.as_ref().map_or(false, |mc| {
                            mc.source_aliases
                                .iter()
                                .any(|a| a.eq_ignore_ascii_case(&table.alias))
                        });
                        if let Some(mc) = mc.filter(|_| is_merge_source) {
                            projection_exprs.push(build_coalesce_for_merge(mc));
                        } else {
                            let combined_idx = source_offsets[table_idx] + col_idx;
                            projection_exprs.push(Expr::Identifier(Ident::new(
                                final_schema.columns[combined_idx].name.clone(),
                            )));
                        }
                    }
                }
                SelectItem::UnnamedExpr(expr) => {
                    projection_exprs.push(rewrite_for_using_join(
                        expr,
                        &table_aliases,
                        &merge_columns,
                    )?);
                }
                SelectItem::ExprWithAlias { expr, alias } => {
                    let rewritten = rewrite_for_using_join(expr, &table_aliases, &merge_columns)?;
                    alias_exprs.insert(alias.value.to_lowercase(), rewritten.clone());
                    projection_exprs.push(rewritten);
                }
            }
        }

        for o in &query.order_by {
            let expr = if let Expr::Identifier(ident) = &o.expr {
                if let Some(e) = alias_exprs.get(&ident.value.to_lowercase()) {
                    e.clone()
                } else {
                    rewrite_for_using_join(&o.expr, &table_aliases, &merge_columns)?
                }
            } else if let Expr::Value(sqlparser::ast::Value::Number(n, _)) = &o.expr {
                if let Ok(pos) = n.parse::<usize>() {
                    if pos == 0 || pos > projection_exprs.len() {
                        return Err(anyhow!("ORDER BY position {} is not in select list", pos));
                    }
                    projection_exprs[pos - 1].clone()
                } else {
                    rewrite_for_using_join(&o.expr, &table_aliases, &merge_columns)?
                }
            } else {
                rewrite_for_using_join(&o.expr, &table_aliases, &merge_columns)?
            };
            rewritten_order_by.push(sqlparser::ast::OrderByExpr {
                expr,
                asc: o.asc,
                nulls_first: o.nulls_first,
            });
        }

        if !rewritten_order_by.is_empty() {
            running_op = Box::new(SortOperator::new(running_op, rewritten_order_by));
        }

        let limit = extract_limit(query);
        let offset = extract_offset(query);
        if limit.is_some() || offset > 0 {
            running_op = Box::new(LimitOperator::new(running_op, limit, offset));
        }

        let rows = execute_operator_tree(
            &mut running_op,
            txn,
            self.store(),
            db_id,
            search_path,
            sequence_values,
        )
        .await?;

        let (columns, column_types, projected_rows) = projection::project_join_output(
            rows,
            &select,
            &resolved_projection,
            &final_schema,
            &table_aliases,
            &merge_columns,
            &wildcard_plan,
            &tables,
            &source_offsets,
        )?;

        Ok(Some(ExecuteResult::Select {
            columns,
            column_types: Some(column_types),
            rows: projected_rows,
            timezone: crate::session_context::current_timezone(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::ast::SetExpr;
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    #[test]
    fn test_not_exists_rewrite_is_not_decorrelated() {
        // Test that NOT (EXISTS ...) is NOT rewritten to NOT (IN ...)
        // because IN can return NULL and get filtered incorrectly.
        let sql = "SELECT * FROM orders o WHERE NOT (EXISTS (SELECT 1 FROM customers c WHERE c.id = o.customer_id))";
        let dialect = PostgreSqlDialect {};
        let ast = Parser::parse_sql(&dialect, sql).expect("Failed to parse SQL");
        let query = match &ast[0] {
            sqlparser::ast::Statement::Query(q) => q.as_ref(),
            _ => panic!("Expected Query statement"),
        };
        let select = match query.body.as_ref() {
            SetExpr::Select(s) => s.as_ref(),
            _ => panic!("Expected Select"),
        };
        let where_expr = select.selection.as_ref().expect("Expected WHERE clause");

        let join_aliases = vec!["o".to_string()];
        let rewritten =
            rewrite_supported_correlated_exists_in_join_filter(where_expr, &join_aliases);

        // The rewritten expression should still be NOT (EXISTS ...), not NOT (IN ...)
        match &rewritten {
            Expr::UnaryOp {
                op: sqlparser::ast::UnaryOperator::Not,
                expr,
            } => {
                match expr.as_ref() {
                    Expr::Nested(inner) => {
                        // It's ok if EXISTS is wrapped in Nested
                        assert!(
                            matches!(inner.as_ref(), Expr::Exists { .. }),
                            "NOT (EXISTS ...) should not be rewritten to NOT (IN ...)"
                        );
                    }
                    Expr::Exists { .. } => {
                        // Good: NOT (EXISTS ...) was preserved
                    }
                    Expr::InSubquery { .. } => {
                        panic!("NOT (EXISTS ...) should NOT be rewritten to NOT (IN ...) due to NULL handling issues");
                    }
                    other => {
                        panic!("Unexpected expression inside NOT: {:?}", other);
                    }
                }
            }
            other => {
                panic!("Expected NOT operator at top level, got: {:?}", other);
            }
        }
    }

    #[test]
    fn test_plain_exists_rewrite_is_decorrelated() {
        // Test that plain EXISTS (without NOT) IS rewritten to IN
        let sql = "SELECT * FROM orders o WHERE EXISTS (SELECT 1 FROM customers c WHERE c.id = o.customer_id)";
        let dialect = PostgreSqlDialect {};
        let ast = Parser::parse_sql(&dialect, sql).expect("Failed to parse SQL");
        let query = match &ast[0] {
            sqlparser::ast::Statement::Query(q) => q.as_ref(),
            _ => panic!("Expected Query statement"),
        };
        let select = match query.body.as_ref() {
            SetExpr::Select(s) => s.as_ref(),
            _ => panic!("Expected Select"),
        };
        let where_expr = select.selection.as_ref().expect("Expected WHERE clause");

        let join_aliases = vec!["o".to_string()];
        let rewritten =
            rewrite_supported_correlated_exists_in_join_filter(where_expr, &join_aliases);

        // The rewritten expression should be IN, not EXISTS
        assert!(
            matches!(rewritten, Expr::InSubquery { .. }),
            "Plain EXISTS should be rewritten to IN, got: {:?}",
            rewritten
        );
    }

    #[test]
    fn test_exists_negated_field_not_rewritten() {
        // Test that EXISTS with negated=true in the node itself is not rewritten
        let sql = "SELECT * FROM orders o WHERE NOT EXISTS (SELECT 1 FROM customers c WHERE c.id = o.customer_id)";
        let dialect = PostgreSqlDialect {};
        let ast = Parser::parse_sql(&dialect, sql).expect("Failed to parse SQL");
        let query = match &ast[0] {
            sqlparser::ast::Statement::Query(q) => q.as_ref(),
            _ => panic!("Expected Query statement"),
        };
        let select = match query.body.as_ref() {
            SetExpr::Select(s) => s.as_ref(),
            _ => panic!("Expected Select"),
        };
        let where_expr = select.selection.as_ref().expect("Expected WHERE clause");

        let join_aliases = vec!["o".to_string()];
        let rewritten =
            rewrite_supported_correlated_exists_in_join_filter(where_expr, &join_aliases);

        // Should not be rewritten to IN
        fn contains_in_subquery(expr: &Expr) -> bool {
            match expr {
                Expr::InSubquery { .. } => true,
                Expr::BinaryOp { left, right, .. } => {
                    contains_in_subquery(left) || contains_in_subquery(right)
                }
                Expr::UnaryOp { expr: inner, .. } | Expr::Nested(inner) => {
                    contains_in_subquery(inner)
                }
                _ => false,
            }
        }

        assert!(
            !contains_in_subquery(&rewritten),
            "NOT EXISTS should not be rewritten to IN"
        );
    }
}
