//! Exhaustive AST walker for sqlparser 0.40.0.
//!
//! Walks every node that can contain a table reference or subquery.
//! Uses explicit pattern matching (not sqlparser's `Visitor` trait) so we
//! can control CTE scope insertion ordering — the key invariant that fixes
//! #643, #644, #654.

use sqlparser::ast::*;

use super::Binder;
use crate::sql::names;

impl Binder {
    // ── Statement ──────────────────────────────────────────────────────

    pub(super) fn walk_statement(&mut self, stmt: &Statement) {
        match stmt {
            Statement::Query(query) => self.walk_query(query),
            // CREATE VIEW ... AS <query> — the query is what we care about.
            Statement::CreateView { query, .. } => self.walk_query(query),
            _ => {}
        }
    }

    // ── Query (scope boundary) ─────────────────────────────────────────

    /// Walk a Query node. This is the scope boundary: each Query gets its
    /// own BindScope frame for CTE name resolution.
    fn walk_query(&mut self, query: &Query) {
        self.push_scope();

        // Process CTEs in declaration order (sequential visibility).
        if let Some(with) = &query.with {
            for cte in &with.cte_tables {
                let cte_name = names::normalize_ident(&cte.alias.name);

                if with.recursive
                    && Self::cte_body_references_name(&cte.query.body, &cte_name)
                {
                    // Recursive self-referencing CTE: name visible in own body.
                    // Add to scope BEFORE walking body → FROM <name> resolves
                    // to the CTE working table, not a real table.
                    self.current_scope_mut().ctes.insert(cte_name);
                    self.walk_query(&cte.query);
                } else {
                    // Non-recursive (or non-self-referencing in RECURSIVE block):
                    // name NOT visible in own body. Walk body FIRST, then add
                    // to scope → FROM <name> in the body is a real table ref.
                    self.walk_query(&cte.query);
                    self.current_scope_mut().ctes.insert(cte_name);
                }
            }
        }

        self.walk_set_expr(&query.body);

        for ob in &query.order_by {
            self.walk_expr(&ob.expr);
        }
        if let Some(limit) = &query.limit {
            self.walk_expr(limit);
        }
        for expr in &query.limit_by {
            self.walk_expr(expr);
        }
        if let Some(offset) = &query.offset {
            self.walk_expr(&offset.value);
        }
        if let Some(fetch) = &query.fetch {
            if let Some(qty) = &fetch.quantity {
                self.walk_expr(qty);
            }
        }

        self.pop_scope();
    }

    // ── SetExpr ────────────────────────────────────────────────────────

    fn walk_set_expr(&mut self, set_expr: &SetExpr) {
        match set_expr {
            SetExpr::Select(select) => self.walk_select(select),
            SetExpr::Query(query) => self.walk_query(query),
            SetExpr::SetOperation { left, right, .. } => {
                self.walk_set_expr(left);
                self.walk_set_expr(right);
            }
            SetExpr::Values(values) => {
                for row in &values.rows {
                    for expr in row {
                        self.walk_expr(expr);
                    }
                }
            }
            SetExpr::Insert(stmt) => self.walk_statement(stmt),
            SetExpr::Update(stmt) => self.walk_statement(stmt),
            SetExpr::Table(_) => {}
        }
    }

    // ── Select ─────────────────────────────────────────────────────────

    fn walk_select(&mut self, select: &Select) {
        // DISTINCT ON(exprs)
        if let Some(Distinct::On(exprs)) = &select.distinct {
            for expr in exprs {
                self.walk_expr(expr);
            }
        }

        // TOP (MSSQL)
        if let Some(top) = &select.top {
            if let Some(qty) = &top.quantity {
                self.walk_expr(qty);
            }
        }

        // FROM
        for twj in &select.from {
            self.walk_table_with_joins(twj);
        }

        // LATERAL VIEWs (Hive)
        for lv in &select.lateral_views {
            self.walk_expr(&lv.lateral_view);
        }

        // WHERE
        if let Some(selection) = &select.selection {
            self.walk_expr(selection);
        }

        // Projection (SELECT items)
        for item in &select.projection {
            self.walk_select_item(item);
        }

        // GROUP BY
        if let GroupByExpr::Expressions(exprs) = &select.group_by {
            for expr in exprs {
                self.walk_expr(expr);
            }
        }

        // Hive-specific: CLUSTER BY, DISTRIBUTE BY, SORT BY
        for expr in &select.cluster_by {
            self.walk_expr(expr);
        }
        for expr in &select.distribute_by {
            self.walk_expr(expr);
        }
        for expr in &select.sort_by {
            self.walk_expr(expr);
        }

        // HAVING
        if let Some(having) = &select.having {
            self.walk_expr(having);
        }

        // WINDOW AS (named window definitions)
        for NamedWindowDefinition(_, spec) in &select.named_window {
            self.walk_window_spec(spec);
        }

        // QUALIFY (Snowflake)
        if let Some(qualify) = &select.qualify {
            self.walk_expr(qualify);
        }
    }

    // ── SelectItem ─────────────────────────────────────────────────────

    fn walk_select_item(&mut self, item: &SelectItem) {
        match item {
            SelectItem::UnnamedExpr(expr) => self.walk_expr(expr),
            SelectItem::ExprWithAlias { expr, .. } => self.walk_expr(expr),
            SelectItem::QualifiedWildcard(_, _) | SelectItem::Wildcard(_) => {}
        }
    }

    // ── TableWithJoins ─────────────────────────────────────────────────

    fn walk_table_with_joins(&mut self, twj: &TableWithJoins) {
        self.walk_table_factor(&twj.relation);
        for join in &twj.joins {
            self.walk_table_factor(&join.relation);
            self.walk_join_constraint(&join.join_operator);
        }
    }

    fn walk_join_constraint(&mut self, join_op: &JoinOperator) {
        let constraint = match join_op {
            JoinOperator::Inner(c)
            | JoinOperator::LeftOuter(c)
            | JoinOperator::RightOuter(c)
            | JoinOperator::FullOuter(c)
            | JoinOperator::LeftSemi(c)
            | JoinOperator::RightSemi(c)
            | JoinOperator::LeftAnti(c)
            | JoinOperator::RightAnti(c) => c,
            JoinOperator::CrossJoin | JoinOperator::CrossApply | JoinOperator::OuterApply => {
                return
            }
        };
        match constraint {
            JoinConstraint::On(expr) => self.walk_expr(expr),
            JoinConstraint::Using(_) | JoinConstraint::Natural | JoinConstraint::None => {}
        }
    }

    // ── TableFactor ────────────────────────────────────────────────────

    fn walk_table_factor(&mut self, tf: &TableFactor) {
        match tf {
            TableFactor::Table {
                name,
                args,
                with_hints,
                ..
            } => {
                if args.is_some() {
                    // When args is Some, sqlparser parsed `name(args)` — this
                    // is a table-valued function call (e.g. generate_series),
                    // NOT a table reference. Only walk the args.
                    for arg in args.as_ref().unwrap() {
                        self.walk_function_arg(arg);
                    }
                } else {
                    // Real table reference → check CTE scope, record dep.
                    self.check_relation(name);
                }
                for hint in with_hints {
                    self.walk_expr(hint);
                }
            }
            TableFactor::Derived { subquery, .. } => {
                self.walk_query(subquery);
            }
            TableFactor::TableFunction { expr, .. } => {
                self.walk_expr(expr);
            }
            TableFactor::Function { args, .. } => {
                // Functions in FROM (e.g. generate_series, unnest, json_each)
                // are function calls, NOT table dependencies. Only walk args
                // for potential subquery expressions.
                for arg in args {
                    self.walk_function_arg(arg);
                }
            }
            TableFactor::UNNEST { array_exprs, .. } => {
                for expr in array_exprs {
                    self.walk_expr(expr);
                }
            }
            TableFactor::NestedJoin {
                table_with_joins, ..
            } => {
                self.walk_table_with_joins(table_with_joins);
            }
            TableFactor::Pivot {
                table,
                aggregate_function,
                ..
            } => {
                self.walk_table_factor(table);
                self.walk_expr(aggregate_function);
            }
            TableFactor::Unpivot { table, .. } => {
                self.walk_table_factor(table);
            }
        }
    }

    // ── FunctionArg ────────────────────────────────────────────────────

    fn walk_function_arg(&mut self, arg: &FunctionArg) {
        match arg {
            FunctionArg::Named { arg, .. } | FunctionArg::Unnamed(arg) => {
                self.walk_function_arg_expr(arg);
            }
        }
    }

    fn walk_function_arg_expr(&mut self, arg: &FunctionArgExpr) {
        match arg {
            FunctionArgExpr::Expr(expr) => self.walk_expr(expr),
            FunctionArgExpr::QualifiedWildcard(_) | FunctionArgExpr::Wildcard => {}
        }
    }

    // ── WindowSpec ─────────────────────────────────────────────────────

    fn walk_window_spec(&mut self, spec: &WindowSpec) {
        for expr in &spec.partition_by {
            self.walk_expr(expr);
        }
        for ob in &spec.order_by {
            self.walk_expr(&ob.expr);
        }
    }

    fn walk_window_type(&mut self, wt: &WindowType) {
        match wt {
            WindowType::WindowSpec(spec) => self.walk_window_spec(spec),
            WindowType::NamedWindow(_) => {}
        }
    }

    // ── Expr (exhaustive) ──────────────────────────────────────────────

    fn walk_expr(&mut self, expr: &Expr) {
        match expr {
            // ── Leaf nodes (no child Expr or Query) ────────────
            Expr::Identifier(_)
            | Expr::CompoundIdentifier(_)
            | Expr::Value(_)
            | Expr::IntroducedString { .. }
            | Expr::TypedString { .. }
            | Expr::MatchAgainst { .. } => {}

            // ── Subquery-containing variants ───────────────────
            Expr::Subquery(query) => self.walk_query(query),
            Expr::Exists { subquery, .. } => self.walk_query(subquery),
            Expr::InSubquery {
                expr, subquery, ..
            } => {
                self.walk_expr(expr);
                self.walk_query(subquery);
            }
            Expr::ArraySubquery(query) => self.walk_query(query),

            // ── Function call ──────────────────────────────────
            Expr::Function(func) => {
                for arg in &func.args {
                    self.walk_function_arg(arg);
                }
                if let Some(filter) = &func.filter {
                    self.walk_expr(filter);
                }
                if let Some(over) = &func.over {
                    self.walk_window_type(over);
                }
                for ob in &func.order_by {
                    self.walk_expr(&ob.expr);
                }
            }

            // ── Single child Expr ──────────────────────────────
            Expr::IsFalse(e)
            | Expr::IsNotFalse(e)
            | Expr::IsTrue(e)
            | Expr::IsNotTrue(e)
            | Expr::IsNull(e)
            | Expr::IsNotNull(e)
            | Expr::IsUnknown(e)
            | Expr::IsNotUnknown(e)
            | Expr::UnaryOp { expr: e, .. }
            | Expr::Nested(e)
            | Expr::CompositeAccess { expr: e, .. }
            | Expr::Collate { expr: e, .. }
            | Expr::Named { expr: e, .. }
            | Expr::AtTimeZone { timestamp: e, .. }
            | Expr::Extract { expr: e, .. }
            | Expr::Ceil { expr: e, .. }
            | Expr::Floor { expr: e, .. } => {
                self.walk_expr(e);
            }

            // ── Cast variants (single child Expr) ─────────────
            Expr::Cast { expr, .. }
            | Expr::TryCast { expr, .. }
            | Expr::SafeCast { expr, .. }
            | Expr::Convert { expr, .. } => {
                self.walk_expr(expr);
            }

            // ── Two child Exprs ────────────────────────────────
            Expr::IsDistinctFrom(a, b) | Expr::IsNotDistinctFrom(a, b) => {
                self.walk_expr(a);
                self.walk_expr(b);
            }
            Expr::BinaryOp { left, right, .. }
            | Expr::JsonAccess { left, right, .. }
            | Expr::AnyOp { left, right, .. }
            | Expr::AllOp { left, right, .. } => {
                self.walk_expr(left);
                self.walk_expr(right);
            }
            Expr::Position { expr, r#in } => {
                self.walk_expr(expr);
                self.walk_expr(r#in);
            }

            // ── Like / pattern variants ────────────────────────
            Expr::Like { expr, pattern, .. }
            | Expr::ILike { expr, pattern, .. }
            | Expr::SimilarTo { expr, pattern, .. } => {
                self.walk_expr(expr);
                self.walk_expr(pattern);
            }
            Expr::RLike { expr, pattern, .. } => {
                self.walk_expr(expr);
                self.walk_expr(pattern);
            }

            // ── Between ────────────────────────────────────────
            Expr::Between {
                expr, low, high, ..
            } => {
                self.walk_expr(expr);
                self.walk_expr(low);
                self.walk_expr(high);
            }

            // ── InList ─────────────────────────────────────────
            Expr::InList { expr, list, .. } => {
                self.walk_expr(expr);
                for e in list {
                    self.walk_expr(e);
                }
            }

            // ── InUnnest ───────────────────────────────────────
            Expr::InUnnest {
                expr, array_expr, ..
            } => {
                self.walk_expr(expr);
                self.walk_expr(array_expr);
            }

            // ── Substring ──────────────────────────────────────
            Expr::Substring {
                expr,
                substring_from,
                substring_for,
                ..
            } => {
                self.walk_expr(expr);
                if let Some(from) = substring_from {
                    self.walk_expr(from);
                }
                if let Some(f) = substring_for {
                    self.walk_expr(f);
                }
            }

            // ── Trim ───────────────────────────────────────────
            Expr::Trim {
                expr,
                trim_what,
                trim_characters,
                ..
            } => {
                self.walk_expr(expr);
                if let Some(what) = trim_what {
                    self.walk_expr(what);
                }
                if let Some(chars) = trim_characters {
                    for c in chars {
                        self.walk_expr(c);
                    }
                }
            }

            // ── Overlay ────────────────────────────────────────
            Expr::Overlay {
                expr,
                overlay_what,
                overlay_from,
                overlay_for,
            } => {
                self.walk_expr(expr);
                self.walk_expr(overlay_what);
                self.walk_expr(overlay_from);
                if let Some(f) = overlay_for {
                    self.walk_expr(f);
                }
            }

            // ── Case ───────────────────────────────────────────
            Expr::Case {
                operand,
                conditions,
                results,
                else_result,
            } => {
                if let Some(op) = operand {
                    self.walk_expr(op);
                }
                for cond in conditions {
                    self.walk_expr(cond);
                }
                for res in results {
                    self.walk_expr(res);
                }
                if let Some(el) = else_result {
                    self.walk_expr(el);
                }
            }

            // ── AggregateExpressionWithFilter ──────────────────
            Expr::AggregateExpressionWithFilter { expr, filter } => {
                self.walk_expr(expr);
                self.walk_expr(filter);
            }

            // ── MapAccess ──────────────────────────────────────
            Expr::MapAccess { column, keys } => {
                self.walk_expr(column);
                for key in keys {
                    self.walk_expr(key);
                }
            }

            // ── ArrayIndex ─────────────────────────────────────
            Expr::ArrayIndex { obj, indexes } => {
                self.walk_expr(obj);
                for idx in indexes {
                    self.walk_expr(idx);
                }
            }

            // ── Collection literals (Vec<Expr>) ────────────────
            Expr::Tuple(exprs) | Expr::Array(Array { elem: exprs, .. }) => {
                for e in exprs {
                    self.walk_expr(e);
                }
            }
            Expr::Struct { values, .. } => {
                for v in values {
                    self.walk_expr(v);
                }
            }

            // ── GROUPING SETS / CUBE / ROLLUP ──────────────────
            Expr::GroupingSets(sets) | Expr::Cube(sets) | Expr::Rollup(sets) => {
                for set in sets {
                    for e in set {
                        self.walk_expr(e);
                    }
                }
            }

            // ── ListAgg ────────────────────────────────────────
            Expr::ListAgg(ListAgg {
                expr,
                separator,
                within_group,
                ..
            }) => {
                self.walk_expr(expr);
                if let Some(sep) = separator {
                    self.walk_expr(sep);
                }
                for ob in within_group {
                    self.walk_expr(&ob.expr);
                }
            }

            // ── ArrayAgg ───────────────────────────────────────
            Expr::ArrayAgg(ArrayAgg {
                expr,
                order_by,
                limit,
                ..
            }) => {
                self.walk_expr(expr);
                if let Some(obs) = order_by {
                    for ob in obs {
                        self.walk_expr(&ob.expr);
                    }
                }
                if let Some(lim) = limit {
                    self.walk_expr(lim);
                }
            }

            // ── Interval ───────────────────────────────────────
            Expr::Interval(Interval { value, .. }) => {
                self.walk_expr(value);
            }
        }
    }
}
