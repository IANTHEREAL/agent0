//! Query-level analysis: FROM, JOIN, WHERE, GROUP BY, HAVING, SELECT, ORDER BY.
//!
//! Builds `AnalyzedQuery` from `sqlparser::ast::Query` by analyzing each clause
//! in dependency order: CTEs → FROM → WHERE → GROUP BY → HAVING → SELECT → ORDER BY.
//!
//! Scope lifecycle: each SELECT pushes one scope that holds both CTE schemas and
//! FROM columns. ORDER BY/LIMIT/OFFSET are analyzed within the same scope so they
//! can reference FROM columns (PostgreSQL semantics).

use sqlparser::ast::{
    self as ast, Expr, Query, Select, SelectItem, SetExpr, TableFactor, TableWithJoins,
};

use crate::sql::names::{normalize_ident, split_object_name};
use crate::sql::table_functions::table_function_key;
use crate::sql::types::coercion::common_type;
use crate::sql::types::CastContext;
use crate::types::{ColumnDef, DataType, TableSchema, Value};
use std::collections::HashSet;

use super::error::AnalyzerError;
use super::scope::Scope;
use super::types::*;
use super::Analyzer;

impl<'a> Analyzer<'a> {
    /// Validate that an expression has Boolean type (for WHERE, HAVING, JOIN ON, CASE WHEN).
    ///
    /// The Analyzer contract requires all boolean contexts to be validated at
    /// analysis time. This catches errors like `WHERE text_column` before any
    /// row is touched.
    fn ensure_boolean(
        &mut self,
        expr: TypedExpr,
        context: &str,
    ) -> Result<TypedExpr, AnalyzerError> {
        if expr.data_type == DataType::Boolean {
            return Ok(expr);
        }

        // PostgreSQL-style boolean context coercion:
        // - NULL in predicate context is allowed (NULL::bool => unknown)
        // - string literals are UNKNOWN and may be cast to bool ('true'/'false')
        // - unresolved parameters resolve to Boolean
        if expr.is_null_constant() {
            return Ok(TypedExpr::null(DataType::Boolean));
        }
        if let TypedExprKind::Parameter { index } = &expr.kind {
            let was_unresolved = self.is_unresolved_param(&expr);
            self.resolve_param_type(*index, &DataType::Boolean)?;
            if was_unresolved {
                return Ok(TypedExpr::new(
                    TypedExprKind::Parameter { index: *index },
                    DataType::Boolean,
                ));
            }
        }
        if matches!(&expr.kind, TypedExprKind::Constant(Value::Text(_))) {
            return Ok(TypedExpr::new(
                TypedExprKind::Cast {
                    expr: Box::new(expr),
                    target_type: DataType::Boolean,
                    cast_context: CastContext::Implicit,
                },
                DataType::Boolean,
            ));
        }

        Err(AnalyzerError::TypeMismatch {
            expected: DataType::Boolean,
            found: expr.data_type,
            context: context.to_string(),
        })
    }

    /// Analyze a complete SQL query (top-level entry point).
    ///
    /// Handles WITH clause (CTEs), query body (SELECT / set operations),
    /// and query-level ORDER BY / LIMIT / OFFSET.
    pub fn analyze_query(&mut self, query: &Query) -> Result<AnalyzedQuery, AnalyzerError> {
        match &*query.body {
            SetExpr::Select(select) => {
                // GROUPING SETS/CUBE/ROLLUP are rewritten into UNION ALL over
                // simple GROUP BY arms so the analyzed pipeline remains single-path.
                if let Some(rewritten) = self.maybe_rewrite_grouping_sets_query(query, select)? {
                    return self.analyze_query(&rewritten);
                }

                // For a simple SELECT, use the combined path that handles
                // CTEs + FROM + ORDER BY all within one scope.
                self.analyze_select_complete(
                    select,
                    query.with.as_ref(),
                    &query.order_by,
                    &query.limit,
                    &query.offset,
                )
            }
            SetExpr::Values(values) => self.analyze_values_complete(
                values,
                query.with.as_ref(),
                &query.order_by,
                &query.limit,
                &query.offset,
            ),

            SetExpr::SetOperation {
                op,
                set_quantifier,
                left,
                right,
            } => {
                // Push scope for CTE visibility and output column ORDER BY resolution.
                self.scopes.push(Scope::new());

                let ctes = self.analyze_cte_definitions(query.with.as_ref())?;

                let left_query = self.analyze_set_expr(left)?;
                let right_query = self.analyze_set_expr(right)?;

                let all = matches!(
                    set_quantifier,
                    ast::SetQuantifier::All | ast::SetQuantifier::AllByName
                );

                let set_op_kind = match op {
                    ast::SetOperator::Union => SetOpKind::Union,
                    ast::SetOperator::Intersect => SetOpKind::Intersect,
                    ast::SetOperator::Except => SetOpKind::Except,
                };

                // Validate column counts match and unify types.
                let output_schema =
                    self.unify_set_operation_schemas(&left_query, &right_query, set_op_kind)?;

                // Coerce both arms to the unified output schema by wrapping each arm
                // in a projection that inserts implicit casts where needed.
                //
                // This matches PostgreSQL planning semantics: each arm is evaluated
                // independently (including its own ORDER BY/LIMIT), then coerced
                // before the set operation combines rows.
                let left_query =
                    self.wrap_set_op_arm_with_coercion(left_query, &output_schema, "__setop_l");
                let right_query =
                    self.wrap_set_op_arm_with_coercion(right_query, &output_schema, "__setop_r");

                // Add output columns to scope for ORDER BY resolution.
                // Set operation ORDER BY references output column names, not FROM columns.
                for (name, dt) in &output_schema {
                    self.scopes
                        .current_mut()
                        .add_column(None, name, dt.clone(), true);
                }

                let analyzed_order_by = self.analyze_order_by_exprs(&query.order_by, &[])?;
                let analyzed_limit = match &query.limit {
                    Some(limit) => Some(self.analyze_expr(limit)?),
                    None => None,
                };
                let analyzed_offset = match &query.offset {
                    Some(offset) => Some(self.analyze_expr(&offset.value)?),
                    None => None,
                };

                self.scopes.pop();

                Ok(AnalyzedQuery {
                    ctes,
                    body: AnalyzedQueryBody::SetOperation {
                        op: set_op_kind,
                        all,
                        left: Box::new(left_query),
                        right: Box::new(right_query),
                    },
                    order_by: analyzed_order_by,
                    limit: analyzed_limit,
                    offset: analyzed_offset,
                    output_schema,
                })
            }

            SetExpr::Query(inner) => self.analyze_query(inner),

            _ => Err(AnalyzerError::Unsupported(format!(
                "query body type: {:?}",
                std::mem::discriminant(&*query.body),
            ))),
        }
    }

    fn maybe_rewrite_grouping_sets_query(
        &self,
        query: &Query,
        select: &Select,
    ) -> Result<Option<Query>, AnalyzerError> {
        let grouping_sets = match &select.group_by {
            ast::GroupByExpr::Expressions(exprs) if exprs.len() == 1 => match &exprs[0] {
                Expr::GroupingSets(sets) => sets.clone(),
                Expr::Rollup(items) => Self::expand_rollup_sets(items),
                Expr::Cube(items) => Self::expand_cube_sets(items),
                _ => return Ok(None),
            },
            _ => return Ok(None),
        };

        let mut set_arms = Vec::with_capacity(grouping_sets.len());
        for group_set in grouping_sets {
            let grouped_keys = self.build_grouped_key_set(&group_set)?;
            let projection = select
                .projection
                .iter()
                .map(|item| self.rewrite_select_item_for_group_set(item, &grouped_keys))
                .collect::<Result<Vec<_>, _>>()?;
            let having = match &select.having {
                Some(expr) => Some(self.rewrite_expr_for_group_set(expr, &grouped_keys)?),
                None => None,
            };

            let mut arm = select.clone();
            arm.projection = projection;
            arm.having = having;
            arm.group_by = ast::GroupByExpr::Expressions(group_set);
            set_arms.push(SetExpr::Select(Box::new(arm)));
        }

        if set_arms.is_empty() {
            return Ok(None);
        }

        let mut iter = set_arms.into_iter();
        let first = iter.next().expect("set_arms is non-empty");
        let body = iter.fold(first, |left, right| SetExpr::SetOperation {
            op: ast::SetOperator::Union,
            set_quantifier: ast::SetQuantifier::All,
            left: Box::new(left),
            right: Box::new(right),
        });

        Ok(Some(Query {
            with: query.with.clone(),
            body: Box::new(body),
            order_by: query.order_by.clone(),
            limit: query.limit.clone(),
            limit_by: query.limit_by.clone(),
            offset: query.offset.clone(),
            fetch: query.fetch.clone(),
            locks: query.locks.clone(),
            for_clause: query.for_clause.clone(),
        }))
    }

    fn expand_rollup_sets(items: &[Vec<Expr>]) -> Vec<Vec<Expr>> {
        let mut sets = Vec::with_capacity(items.len() + 1);
        for keep in (0..=items.len()).rev() {
            let mut set = Vec::new();
            for item in items.iter().take(keep) {
                set.extend(item.clone());
            }
            sets.push(set);
        }
        sets
    }

    fn expand_cube_sets(items: &[Vec<Expr>]) -> Vec<Vec<Expr>> {
        let total = 1usize << items.len();
        let mut sets = Vec::with_capacity(total);
        for mask in (0..total).rev() {
            let mut set = Vec::new();
            for (idx, item) in items.iter().enumerate() {
                if mask & (1usize << idx) != 0 {
                    set.extend(item.clone());
                }
            }
            sets.push(set);
        }
        sets
    }

    fn build_grouped_key_set(&self, set: &[Expr]) -> Result<HashSet<String>, AnalyzerError> {
        let mut keys = HashSet::new();
        for expr in set {
            let Some(expr_keys) = Self::column_expr_keys(expr) else {
                return Err(AnalyzerError::Unsupported(
                    "GROUPING SETS currently supports column references only".to_string(),
                ));
            };
            keys.extend(expr_keys);
        }
        Ok(keys)
    }

    fn column_expr_keys(expr: &Expr) -> Option<Vec<String>> {
        match expr {
            Expr::Identifier(ident) => Some(vec![normalize_ident(ident)]),
            Expr::CompoundIdentifier(parts) => {
                if parts.is_empty() {
                    return None;
                }
                let full = parts
                    .iter()
                    .map(normalize_ident)
                    .collect::<Vec<_>>()
                    .join(".");
                let short = normalize_ident(parts.last()?);
                if full == short {
                    Some(vec![short])
                } else {
                    Some(vec![full, short])
                }
            }
            Expr::Nested(inner) => Self::column_expr_keys(inner),
            _ => None,
        }
    }

    fn expr_is_grouped_column(grouped_keys: &HashSet<String>, expr: &Expr) -> bool {
        Self::column_expr_keys(expr)
            .map(|keys| keys.into_iter().any(|k| grouped_keys.contains(&k)))
            .unwrap_or(false)
    }

    fn default_alias_for_expr(expr: &Expr) -> Option<ast::Ident> {
        match expr {
            Expr::Identifier(ident) => Some(ident.clone()),
            Expr::CompoundIdentifier(parts) => parts.last().cloned(),
            _ => None,
        }
    }

    fn rewrite_select_item_for_group_set(
        &self,
        item: &SelectItem,
        grouped_keys: &HashSet<String>,
    ) -> Result<SelectItem, AnalyzerError> {
        match item {
            SelectItem::UnnamedExpr(expr) => {
                let rewritten = self.rewrite_expr_for_group_set(expr, grouped_keys)?;
                if matches!(rewritten, Expr::Value(ast::Value::Null)) {
                    if let Some(alias) = Self::default_alias_for_expr(expr) {
                        return Ok(SelectItem::ExprWithAlias {
                            expr: rewritten,
                            alias,
                        });
                    }
                }
                Ok(SelectItem::UnnamedExpr(rewritten))
            }
            SelectItem::ExprWithAlias { expr, alias } => Ok(SelectItem::ExprWithAlias {
                expr: self.rewrite_expr_for_group_set(expr, grouped_keys)?,
                alias: alias.clone(),
            }),
            SelectItem::QualifiedWildcard(_, _) | SelectItem::Wildcard(_) => {
                Err(AnalyzerError::Unsupported(
                    "GROUPING SETS with wildcard projection is not yet supported".to_string(),
                ))
            }
        }
    }

    fn rewrite_expr_for_group_set(
        &self,
        expr: &Expr,
        grouped_keys: &HashSet<String>,
    ) -> Result<Expr, AnalyzerError> {
        match expr {
            Expr::Identifier(_) | Expr::CompoundIdentifier(_) => {
                if Self::expr_is_grouped_column(grouped_keys, expr) {
                    Ok(expr.clone())
                } else {
                    Ok(Expr::Value(ast::Value::Null))
                }
            }
            Expr::BinaryOp { left, op, right } => Ok(Expr::BinaryOp {
                left: Box::new(self.rewrite_expr_for_group_set(left, grouped_keys)?),
                op: op.clone(),
                right: Box::new(self.rewrite_expr_for_group_set(right, grouped_keys)?),
            }),
            Expr::UnaryOp { op, expr } => Ok(Expr::UnaryOp {
                op: op.clone(),
                expr: Box::new(self.rewrite_expr_for_group_set(expr, grouped_keys)?),
            }),
            Expr::Nested(inner) => Ok(Expr::Nested(Box::new(
                self.rewrite_expr_for_group_set(inner, grouped_keys)?,
            ))),
            Expr::IsFalse(inner) => Ok(Expr::IsFalse(Box::new(
                self.rewrite_expr_for_group_set(inner, grouped_keys)?,
            ))),
            Expr::IsNotFalse(inner) => Ok(Expr::IsNotFalse(Box::new(
                self.rewrite_expr_for_group_set(inner, grouped_keys)?,
            ))),
            Expr::IsTrue(inner) => Ok(Expr::IsTrue(Box::new(
                self.rewrite_expr_for_group_set(inner, grouped_keys)?,
            ))),
            Expr::IsNotTrue(inner) => Ok(Expr::IsNotTrue(Box::new(
                self.rewrite_expr_for_group_set(inner, grouped_keys)?,
            ))),
            Expr::IsNull(inner) => Ok(Expr::IsNull(Box::new(
                self.rewrite_expr_for_group_set(inner, grouped_keys)?,
            ))),
            Expr::IsNotNull(inner) => Ok(Expr::IsNotNull(Box::new(
                self.rewrite_expr_for_group_set(inner, grouped_keys)?,
            ))),
            Expr::IsUnknown(inner) => Ok(Expr::IsUnknown(Box::new(
                self.rewrite_expr_for_group_set(inner, grouped_keys)?,
            ))),
            Expr::IsNotUnknown(inner) => Ok(Expr::IsNotUnknown(Box::new(
                self.rewrite_expr_for_group_set(inner, grouped_keys)?,
            ))),
            Expr::IsDistinctFrom(left, right) => Ok(Expr::IsDistinctFrom(
                Box::new(self.rewrite_expr_for_group_set(left, grouped_keys)?),
                Box::new(self.rewrite_expr_for_group_set(right, grouped_keys)?),
            )),
            Expr::IsNotDistinctFrom(left, right) => Ok(Expr::IsNotDistinctFrom(
                Box::new(self.rewrite_expr_for_group_set(left, grouped_keys)?),
                Box::new(self.rewrite_expr_for_group_set(right, grouped_keys)?),
            )),
            Expr::Between {
                expr,
                negated,
                low,
                high,
            } => Ok(Expr::Between {
                expr: Box::new(self.rewrite_expr_for_group_set(expr, grouped_keys)?),
                negated: *negated,
                low: Box::new(self.rewrite_expr_for_group_set(low, grouped_keys)?),
                high: Box::new(self.rewrite_expr_for_group_set(high, grouped_keys)?),
            }),
            Expr::InList {
                expr,
                list,
                negated,
            } => Ok(Expr::InList {
                expr: Box::new(self.rewrite_expr_for_group_set(expr, grouped_keys)?),
                list: list
                    .iter()
                    .map(|e| self.rewrite_expr_for_group_set(e, grouped_keys))
                    .collect::<Result<Vec<_>, _>>()?,
                negated: *negated,
            }),
            Expr::Like {
                negated,
                expr,
                pattern,
                escape_char,
            } => Ok(Expr::Like {
                negated: *negated,
                expr: Box::new(self.rewrite_expr_for_group_set(expr, grouped_keys)?),
                pattern: Box::new(self.rewrite_expr_for_group_set(pattern, grouped_keys)?),
                escape_char: *escape_char,
            }),
            Expr::ILike {
                negated,
                expr,
                pattern,
                escape_char,
            } => Ok(Expr::ILike {
                negated: *negated,
                expr: Box::new(self.rewrite_expr_for_group_set(expr, grouped_keys)?),
                pattern: Box::new(self.rewrite_expr_for_group_set(pattern, grouped_keys)?),
                escape_char: *escape_char,
            }),
            Expr::SimilarTo {
                negated,
                expr,
                pattern,
                escape_char,
            } => Ok(Expr::SimilarTo {
                negated: *negated,
                expr: Box::new(self.rewrite_expr_for_group_set(expr, grouped_keys)?),
                pattern: Box::new(self.rewrite_expr_for_group_set(pattern, grouped_keys)?),
                escape_char: *escape_char,
            }),
            Expr::RLike {
                negated,
                expr,
                pattern,
                regexp,
            } => Ok(Expr::RLike {
                negated: *negated,
                expr: Box::new(self.rewrite_expr_for_group_set(expr, grouped_keys)?),
                pattern: Box::new(self.rewrite_expr_for_group_set(pattern, grouped_keys)?),
                regexp: *regexp,
            }),
            Expr::AnyOp {
                left,
                compare_op,
                right,
            } => Ok(Expr::AnyOp {
                left: Box::new(self.rewrite_expr_for_group_set(left, grouped_keys)?),
                compare_op: compare_op.clone(),
                right: Box::new(self.rewrite_expr_for_group_set(right, grouped_keys)?),
            }),
            Expr::AllOp {
                left,
                compare_op,
                right,
            } => Ok(Expr::AllOp {
                left: Box::new(self.rewrite_expr_for_group_set(left, grouped_keys)?),
                compare_op: compare_op.clone(),
                right: Box::new(self.rewrite_expr_for_group_set(right, grouped_keys)?),
            }),
            Expr::Case {
                operand,
                conditions,
                results,
                else_result,
            } => Ok(Expr::Case {
                operand: match operand {
                    Some(e) => Some(Box::new(self.rewrite_expr_for_group_set(e, grouped_keys)?)),
                    None => None,
                },
                conditions: conditions
                    .iter()
                    .map(|e| self.rewrite_expr_for_group_set(e, grouped_keys))
                    .collect::<Result<Vec<_>, _>>()?,
                results: results
                    .iter()
                    .map(|e| self.rewrite_expr_for_group_set(e, grouped_keys))
                    .collect::<Result<Vec<_>, _>>()?,
                else_result: match else_result {
                    Some(e) => Some(Box::new(self.rewrite_expr_for_group_set(e, grouped_keys)?)),
                    None => None,
                },
            }),
            Expr::Cast {
                expr,
                data_type,
                format,
            } => Ok(Expr::Cast {
                expr: Box::new(self.rewrite_expr_for_group_set(expr, grouped_keys)?),
                data_type: data_type.clone(),
                format: format.clone(),
            }),
            Expr::TryCast {
                expr,
                data_type,
                format,
            } => Ok(Expr::TryCast {
                expr: Box::new(self.rewrite_expr_for_group_set(expr, grouped_keys)?),
                data_type: data_type.clone(),
                format: format.clone(),
            }),
            Expr::SafeCast {
                expr,
                data_type,
                format,
            } => Ok(Expr::SafeCast {
                expr: Box::new(self.rewrite_expr_for_group_set(expr, grouped_keys)?),
                data_type: data_type.clone(),
                format: format.clone(),
            }),
            Expr::Function(func) => self.rewrite_function_for_group_set(func, grouped_keys),
            _ => Ok(expr.clone()),
        }
    }

    fn rewrite_function_for_group_set(
        &self,
        func: &ast::Function,
        grouped_keys: &HashSet<String>,
    ) -> Result<Expr, AnalyzerError> {
        let func_name = func.name.0.last().map(normalize_ident).unwrap_or_default();

        if func_name.eq_ignore_ascii_case("GROUPING") {
            if func.args.is_empty() {
                return Err(AnalyzerError::Unsupported(
                    "GROUPING() requires at least one argument".to_string(),
                ));
            }
            let mut mask = 0i32;
            let arity = func.args.len();
            for (idx, arg) in func.args.iter().enumerate() {
                let expr = match arg {
                    ast::FunctionArg::Unnamed(ast::FunctionArgExpr::Expr(e)) => e,
                    ast::FunctionArg::Named {
                        arg: ast::FunctionArgExpr::Expr(e),
                        ..
                    } => e,
                    _ => {
                        return Err(AnalyzerError::Unsupported(
                            "GROUPING() arguments must be expressions".to_string(),
                        ))
                    }
                };

                let Some(arg_keys) = Self::column_expr_keys(expr) else {
                    return Err(AnalyzerError::Unsupported(
                        "GROUPING() arguments must be column references".to_string(),
                    ));
                };
                let is_grouped = arg_keys.iter().any(|k| grouped_keys.contains(k));
                if !is_grouped {
                    let bit = (arity - idx - 1) as i32;
                    mask |= 1 << bit;
                }
            }
            return Ok(Expr::Value(ast::Value::Number(mask.to_string(), false)));
        }

        let mut cloned = func.clone();
        if let Some(filter) = &func.filter {
            cloned.filter = Some(Box::new(
                self.rewrite_expr_for_group_set(filter, grouped_keys)?,
            ));
        }
        Ok(Expr::Function(cloned))
    }

    /// Analyze a SetExpr (used for set operations' left/right branches).
    ///
    /// Each branch manages its own scope lifecycle independently.
    fn analyze_set_expr(&mut self, set_expr: &SetExpr) -> Result<AnalyzedQuery, AnalyzerError> {
        match set_expr {
            SetExpr::Select(select) => self.analyze_select(select),
            SetExpr::Values(values) => self.analyze_values(values),
            SetExpr::Query(query) => self.analyze_query(query),
            SetExpr::SetOperation {
                op,
                set_quantifier,
                left,
                right,
            } => {
                let left_query = self.analyze_set_expr(left)?;
                let right_query = self.analyze_set_expr(right)?;
                let all = matches!(
                    set_quantifier,
                    ast::SetQuantifier::All | ast::SetQuantifier::AllByName
                );
                let set_op_kind = match op {
                    ast::SetOperator::Union => SetOpKind::Union,
                    ast::SetOperator::Intersect => SetOpKind::Intersect,
                    ast::SetOperator::Except => SetOpKind::Except,
                };
                // Validate column counts match and unify types.
                let output_schema =
                    self.unify_set_operation_schemas(&left_query, &right_query, set_op_kind)?;
                let left_query =
                    self.wrap_set_op_arm_with_coercion(left_query, &output_schema, "__setop_l");
                let right_query =
                    self.wrap_set_op_arm_with_coercion(right_query, &output_schema, "__setop_r");
                Ok(AnalyzedQuery {
                    ctes: vec![],
                    body: AnalyzedQueryBody::SetOperation {
                        op: set_op_kind,
                        all,
                        left: Box::new(left_query),
                        right: Box::new(right_query),
                    },
                    order_by: vec![],
                    limit: None,
                    offset: None,
                    output_schema,
                })
            }
            _ => Err(AnalyzerError::Unsupported(
                "unsupported set expression".to_string(),
            )),
        }
    }

    /// Analyze a SELECT with CTEs and ORDER BY/LIMIT/OFFSET — all within one scope.
    ///
    /// This is the primary path for `SELECT ... FROM ... WHERE ... ORDER BY ...`.
    /// CTEs, FROM columns, and ORDER BY share the same scope, ensuring ORDER BY
    /// can reference both output aliases and FROM columns (PostgreSQL semantics).
    fn analyze_select_complete(
        &mut self,
        select: &Select,
        with: Option<&ast::With>,
        order_by: &[ast::OrderByExpr],
        limit: &Option<ast::Expr>,
        offset: &Option<ast::Offset>,
    ) -> Result<AnalyzedQuery, AnalyzerError> {
        // Push scope for this query level.
        // Aggregates and windows start disabled — only enabled for clauses
        // where they are semantically valid (HAVING, SELECT, ORDER BY).
        self.scopes.push(Scope::new());

        // 1. Register CTEs (visible for FROM table resolution)
        let ctes = self.analyze_cte_definitions(with)?;

        // 2. FROM clause → populate scope with table columns
        let from = self.analyze_from(&select.from)?;

        // 3. WHERE clause (must be boolean, aggregates NOT allowed)
        let filter = match &select.selection {
            Some(expr) => {
                let analyzed = self.analyze_expr(expr)?;
                let analyzed = self.ensure_boolean(analyzed, "WHERE clause")?;
                Some(analyzed)
            }
            None => None,
        };

        // 4. GROUP BY (aggregates NOT allowed)
        let group_by = self.analyze_group_by(&select.group_by, &select.projection)?;

        // Enable aggregates and windows for HAVING, SELECT, ORDER BY.
        {
            let scope = self.scopes.current_mut();
            scope.allow_aggregates = true;
            scope.allow_windows = true;
        }

        // 5. HAVING (must be boolean, aggregates allowed)
        let having = match &select.having {
            Some(expr) => {
                let analyzed = self.analyze_expr(expr)?;
                let analyzed = self.ensure_boolean(analyzed, "HAVING clause")?;
                Some(analyzed)
            }
            None => None,
        };

        // 6. SELECT list (aggregates allowed)
        let wildcard_order = Self::build_wildcard_projection_order(select, &from);
        let (projection, output_schema) =
            self.analyze_projection(&select.projection, wildcard_order.as_deref())?;

        // 7. DISTINCT
        let distinct = self.analyze_distinct(&select.distinct)?;

        // 8. ORDER BY (can reference output aliases + FROM columns)
        let analyzed_order_by = self.analyze_order_by_exprs(order_by, &projection)?;

        // Validate grouping semantics AFTER ORDER BY analysis so we can check
        // ORDER BY expressions for ungrouped column references too.
        self.validate_grouping_semantics(
            &group_by,
            &projection,
            having.as_ref(),
            &analyzed_order_by,
        )?;

        // 9. LIMIT — resolve params to Int64
        let analyzed_limit = match limit {
            Some(l) => {
                let mut expr = self.analyze_expr(l)?;
                if let TypedExprKind::Parameter { index } = &expr.kind {
                    let was_unresolved = self.is_unresolved_param(&expr);
                    self.resolve_param_type(*index, &DataType::Int64)?;
                    if was_unresolved {
                        expr = TypedExpr::new(
                            TypedExprKind::Parameter { index: *index },
                            DataType::Int64,
                        );
                    }
                }
                Some(expr)
            }
            None => None,
        };

        // 10. OFFSET — resolve params to Int64
        let analyzed_offset = match offset {
            Some(o) => {
                let mut expr = self.analyze_expr(&o.value)?;
                if let TypedExprKind::Parameter { index } = &expr.kind {
                    let was_unresolved = self.is_unresolved_param(&expr);
                    self.resolve_param_type(*index, &DataType::Int64)?;
                    if was_unresolved {
                        expr = TypedExpr::new(
                            TypedExprKind::Parameter { index: *index },
                            DataType::Int64,
                        );
                    }
                }
                Some(expr)
            }
            None => None,
        };

        // 11. Correlated subqueries in JOIN context are handled by the executor
        // via execute_async_nested_loop_join (per-row materialization fallback).
        // No rejection needed here.

        // Pop scope
        self.scopes.pop();

        let select_body = AnalyzedSelect {
            projection,
            from,
            where_clause: filter,
            group_by,
            having,
            distinct,
        };

        Ok(AnalyzedQuery {
            ctes,
            body: AnalyzedQueryBody::Select(select_body),
            order_by: analyzed_order_by,
            limit: analyzed_limit,
            offset: analyzed_offset,
            output_schema,
        })
    }

    /// Analyze a bare SELECT (no CTEs, no ORDER BY).
    ///
    /// Used by `analyze_set_expr` for each branch of a set operation.
    fn analyze_select(&mut self, select: &Select) -> Result<AnalyzedQuery, AnalyzerError> {
        self.analyze_select_complete(select, None, &[], &None, &None)
    }

    /// Analyze a VALUES query with optional WITH/ORDER BY/LIMIT/OFFSET.
    fn analyze_values_complete(
        &mut self,
        values: &ast::Values,
        with: Option<&ast::With>,
        order_by: &[ast::OrderByExpr],
        limit: &Option<ast::Expr>,
        offset: &Option<ast::Offset>,
    ) -> Result<AnalyzedQuery, AnalyzerError> {
        self.scopes.push(Scope::new());

        let ctes = self.analyze_cte_definitions(with)?;
        let (rows, output_schema) = self.analyze_values_rows(&values.rows)?;

        // Expose VALUES output columns (column1, column2, ...) for ORDER BY resolution.
        for (name, dt) in &output_schema {
            self.scopes
                .current_mut()
                .add_column(None, name, dt.clone(), true);
        }

        // Build synthetic projection so ORDER BY positional refs (ORDER BY 1) map correctly.
        let projection: Vec<AnalyzedProjection> = output_schema
            .iter()
            .enumerate()
            .map(|(idx, (name, dt))| AnalyzedProjection {
                expr: TypedExpr::new(
                    TypedExprKind::ColumnRef {
                        scope_depth: 0,
                        column_index: idx,
                        column_name: name.clone(),
                    },
                    dt.clone(),
                ),
                output_name: name.clone(),
            })
            .collect();

        let analyzed_order_by = self.analyze_order_by_exprs(order_by, &projection)?;
        let analyzed_limit = match limit {
            Some(l) => Some(self.analyze_expr(l)?),
            None => None,
        };
        let analyzed_offset = match offset {
            Some(o) => Some(self.analyze_expr(&o.value)?),
            None => None,
        };

        self.scopes.pop();

        Ok(AnalyzedQuery {
            ctes,
            body: AnalyzedQueryBody::Values(rows),
            order_by: analyzed_order_by,
            limit: analyzed_limit,
            offset: analyzed_offset,
            output_schema,
        })
    }

    /// Analyze a VALUES branch (used inside set operations).
    fn analyze_values(&mut self, values: &ast::Values) -> Result<AnalyzedQuery, AnalyzerError> {
        self.analyze_values_complete(values, None, &[], &None, &None)
    }

    /// Analyze VALUES rows and unify each column's type across all rows.
    fn analyze_values_rows(
        &mut self,
        rows: &[Vec<Expr>],
    ) -> Result<(Vec<Vec<TypedExpr>>, Vec<(String, DataType)>), AnalyzerError> {
        if rows.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }

        let expected_len = rows[0].len();
        for row in rows {
            if row.len() != expected_len {
                return Err(AnalyzerError::Unsupported(
                    "VALUES lists must all be the same length".to_string(),
                ));
            }
        }

        let mut analyzed_rows: Vec<Vec<TypedExpr>> = Vec::with_capacity(rows.len());
        for row in rows {
            let mut analyzed_row = Vec::with_capacity(expected_len);
            for expr in row {
                analyzed_row.push(self.analyze_expr(expr)?);
            }
            analyzed_rows.push(analyzed_row);
        }

        let mut output_schema = Vec::with_capacity(expected_len);
        for col_idx in 0..expected_len {
            let col_exprs: Vec<&TypedExpr> = analyzed_rows.iter().map(|r| &r[col_idx]).collect();

            let non_null_types: Vec<DataType> = col_exprs
                .iter()
                .filter(|e| !e.is_null_constant())
                .map(|e| e.data_type.clone())
                .collect();

            let target_type = if non_null_types.is_empty() {
                DataType::Text
            } else {
                let mut unified = non_null_types[0].clone();
                for dt in non_null_types.iter().skip(1) {
                    unified = common_type(&unified, dt).ok_or_else(|| {
                        AnalyzerError::TypesCannotBeMatched {
                            types: col_exprs.iter().map(|e| e.data_type.clone()).collect(),
                            context: "VALUES".to_string(),
                        }
                    })?;
                }
                unified
            };

            for row in &mut analyzed_rows {
                let expr = row[col_idx].clone();
                row[col_idx] = Self::coerce_values_expr(expr, &target_type);
            }

            output_schema.push((format!("column{}", col_idx + 1), target_type));
        }

        Ok((analyzed_rows, output_schema))
    }

    fn coerce_values_expr(expr: TypedExpr, target: &DataType) -> TypedExpr {
        if expr.data_type == *target {
            expr
        } else if expr.is_null_constant() {
            TypedExpr::null(target.clone())
        } else {
            TypedExpr::new(
                TypedExprKind::Cast {
                    expr: Box::new(expr),
                    target_type: target.clone(),
                    cast_context: CastContext::Implicit,
                },
                target.clone(),
            )
        }
    }

    // ── FROM clause ─────────────────────────────────────────

    fn analyze_from(
        &mut self,
        from: &[TableWithJoins],
    ) -> Result<Vec<AnalyzedTableRef>, AnalyzerError> {
        let mut refs = Vec::new();
        for twj in from {
            let table_ref = self.analyze_table_with_joins(twj)?;
            refs.push(table_ref);
        }
        Ok(refs)
    }

    pub(super) fn analyze_table_with_joins(
        &mut self,
        twj: &TableWithJoins,
    ) -> Result<AnalyzedTableRef, AnalyzerError> {
        // Record where this table group's columns start (excludes preceding comma-FROM items).
        let left_start = self.scopes.current().column_count();
        let mut result = self.analyze_table_factor(&twj.relation)?;

        // Process JOINs left-to-right, each one wraps the accumulated result
        for join in &twj.joins {
            // Record boundary before right table is added to scope.
            // Columns at indices left_start..right_start belong to left, >= right_start to right.
            let right_start = self.scopes.current().column_count();
            let right = self.analyze_table_factor(&join.relation)?;
            let (join_type, condition) =
                self.analyze_join_constraint(&join.join_operator, left_start, right_start)?;

            // USING join: hide right-side duplicate columns from SELECT * expansion.
            // right_index is LOCAL to the right operator; convert to GLOBAL scope index for hiding.
            if let JoinCondition::Using(ref cols) = condition {
                for uc in cols {
                    self.scopes.current_mut().register_using_column(
                        &uc.name,
                        left_start + uc.left_index,
                        right_start + uc.right_index,
                        uc.data_type.clone(),
                        uc.left_type.clone(),
                        uc.right_type.clone(),
                    );
                }
            }

            result = AnalyzedTableRef {
                kind: AnalyzedTableRefKind::Join {
                    left: Box::new(result),
                    right: Box::new(right),
                    join_type,
                    condition,
                    left_col_start: left_start,
                },
                alias: None,
            };
        }

        Ok(result)
    }

    fn analyze_table_factor(
        &mut self,
        factor: &TableFactor,
    ) -> Result<AnalyzedTableRef, AnalyzerError> {
        match factor {
            TableFactor::Table {
                name,
                alias,
                args: Some(func_args),
                ..
            } => {
                // Table-valued function call in FROM.

                let (schema_opt, obj_name) = split_object_name(name)
                    .map_err(|e| AnalyzerError::Unsupported(e.to_string()))?;
                let alias_str = alias
                    .as_ref()
                    .map(|a| normalize_ident(&a.name))
                    .unwrap_or_else(|| obj_name.clone());
                let dispatch_name = match &schema_opt {
                    Some(schema) => format!("{}.{}", schema, obj_name),
                    None => obj_name.clone(),
                };

                // Analyze function arguments, preserving named parameters.
                let mut typed_args = Vec::with_capacity(func_args.len());
                for arg in func_args {
                    match arg {
                        ast::FunctionArg::Unnamed(ast::FunctionArgExpr::Expr(e)) => {
                            typed_args.push(TypedFunctionArg::Positional(self.analyze_expr(e)?));
                        }
                        ast::FunctionArg::Named {
                            name,
                            arg: ast::FunctionArgExpr::Expr(e),
                            ..
                        } => {
                            typed_args.push(TypedFunctionArg::Named {
                                name: crate::sql::names::normalize_ident(name),
                                expr: self.analyze_expr(e)?,
                            });
                        }
                        _ => {
                            return Err(AnalyzerError::Unsupported(
                                "unsupported table function argument".to_string(),
                            ));
                        }
                    }
                }

                let key = table_function_key(name, func_args);
                let mut output_cols: Vec<(String, DataType, bool)> =
                    if let Some(schema) = self.catalog.resolve_table_function(&key) {
                        schema
                            .columns
                            .iter()
                            .map(|c| (c.name.clone(), c.data_type.clone(), c.nullable))
                            .collect()
                    } else if obj_name.eq_ignore_ascii_case("generate_series") {
                        // generate_series(start, stop [, step]) returns a single column.
                        let positional: Vec<&TypedExpr> = typed_args
                            .iter()
                            .filter_map(|a| match a {
                                TypedFunctionArg::Positional(e) => Some(e),
                                _ => None,
                            })
                            .collect();
                        if positional.len() < 2 {
                            return Err(AnalyzerError::Unsupported(
                                "generate_series requires at least 2 arguments".to_string(),
                            ));
                        }

                        let start_ty = &positional[0].data_type;
                        let stop_ty = &positional[1].data_type;
                        let out_ty = if matches!(start_ty, DataType::Date)
                            && matches!(stop_ty, DataType::Date)
                        {
                            DataType::TimestampTz
                        } else if matches!(start_ty, DataType::Timestamp)
                            && matches!(stop_ty, DataType::Timestamp)
                        {
                            DataType::Timestamp
                        } else if matches!(start_ty, DataType::Int32)
                            && matches!(stop_ty, DataType::Int32)
                        {
                            DataType::Int32
                        } else if matches!(start_ty, DataType::Int64)
                            && matches!(stop_ty, DataType::Int64)
                        {
                            DataType::Int64
                        } else if matches!(start_ty, DataType::Float64)
                            && matches!(stop_ty, DataType::Float64)
                        {
                            DataType::Float64
                        } else if matches!(start_ty, DataType::Numeric { .. })
                            && matches!(stop_ty, DataType::Numeric { .. })
                        {
                            start_ty.clone()
                        } else if let Some(common) = common_type(start_ty, stop_ty) {
                            common
                        } else {
                            DataType::Text
                        };

                        // Column name semantics:
                        // - `FROM generate_series(...) AS n` → column name "n"
                        // - `FROM generate_series(...) AS t(col)` → column name "col"
                        let col_name = if let Some(ta) = alias {
                            if !ta.columns.is_empty() {
                                crate::sql::names::normalize_ident(&ta.columns[0])
                            } else {
                                crate::sql::names::normalize_ident(&ta.name)
                            }
                        } else {
                            "generate_series".to_string()
                        };

                        vec![(col_name, out_ty, false)]
                    } else if obj_name.eq_ignore_ascii_case("current_schema")
                        || obj_name.eq_ignore_ascii_case("current_database")
                        || obj_name.eq_ignore_ascii_case("current_user")
                        || obj_name.eq_ignore_ascii_case("session_user")
                        || obj_name.eq_ignore_ascii_case("user")
                    {
                        // Scalar functions used in FROM return a single-row, single-column relation.
                        let col_name = obj_name.to_lowercase();
                        vec![(col_name, DataType::Text, false)]
                    } else {
                        return Err(AnalyzerError::Unsupported(format!(
                            "unsupported table-valued function: {}",
                            obj_name
                        )));
                    };

                // Apply alias column list (renames output columns).
                if let Some(ta) = alias {
                    if !ta.columns.is_empty() {
                        if ta.columns.len() != output_cols.len() {
                            return Err(AnalyzerError::Unsupported(format!(
                                "table function alias column count mismatch: expected {}, got {}",
                                output_cols.len(),
                                ta.columns.len()
                            )));
                        }
                        for (i, ident) in ta.columns.iter().enumerate() {
                            output_cols[i].0 = crate::sql::names::normalize_ident(ident);
                        }
                    }
                }

                self.scopes
                    .current_mut()
                    .add_table(&alias_str, &output_cols);

                let output_columns: Vec<(String, DataType)> = output_cols
                    .iter()
                    .map(|(n, dt, _)| (n.clone(), dt.clone()))
                    .collect();

                let func = ResolvedFunction {
                    name: dispatch_name,
                    kind: FunctionKind::Builtin,
                    return_type: DataType::Text,
                };

                Ok(AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Function {
                        func,
                        args: typed_args,
                        output_columns,
                    },
                    alias: Some(alias_str),
                })
            }

            TableFactor::Table {
                name,
                alias,
                args: None,
                ..
            } => {
                // PostgreSQL SQL value functions have special syntax and can appear in FROM
                // without trailing parentheses (e.g. `FROM CURRENT_USER`).
                //
                // Treat these as scalar table functions (single-row, single-column relation).
                // Quoted identifiers should remain resolvable as real tables.
                if name.0.len() == 1 {
                    let ident = &name.0[0];
                    if ident.quote_style.is_none()
                        && (ident.value.eq_ignore_ascii_case("current_user")
                            || ident.value.eq_ignore_ascii_case("session_user")
                            || ident.value.eq_ignore_ascii_case("user")
                            || ident.value.eq_ignore_ascii_case("current_schema"))
                    {
                        let obj_name = ident.value.to_lowercase();
                        let alias_str = alias
                            .as_ref()
                            .map(|a| normalize_ident(&a.name))
                            .unwrap_or_else(|| obj_name.clone());

                        let mut output_cols: Vec<(String, DataType, bool)> =
                            vec![(obj_name.clone(), DataType::Text, false)];

                        // Apply alias column list (renames output columns).
                        if let Some(ta) = alias {
                            if !ta.columns.is_empty() {
                                if ta.columns.len() != output_cols.len() {
                                    return Err(AnalyzerError::Unsupported(format!(
                                        "table function alias column count mismatch: expected {}, got {}",
                                        output_cols.len(),
                                        ta.columns.len()
                                    )));
                                }
                                for (i, ident) in ta.columns.iter().enumerate() {
                                    output_cols[i].0 = crate::sql::names::normalize_ident(ident);
                                }
                            }
                        }

                        self.scopes
                            .current_mut()
                            .add_table(&alias_str, &output_cols);

                        let output_columns: Vec<(String, DataType)> = output_cols
                            .iter()
                            .map(|(n, dt, _)| (n.clone(), dt.clone()))
                            .collect();

                        let func = ResolvedFunction {
                            name: obj_name.to_ascii_uppercase(),
                            kind: FunctionKind::Builtin,
                            return_type: DataType::Text,
                        };

                        return Ok(AnalyzedTableRef {
                            kind: AnalyzedTableRefKind::Function {
                                func,
                                args: vec![],
                                output_columns,
                            },
                            alias: Some(alias_str),
                        });
                    }
                }

                // Split ObjectName into (optional schema, object name).
                let (schema_opt, obj_name) = split_object_name(name)
                    .map_err(|e| AnalyzerError::Unsupported(e.to_string()))?;
                let alias_str = alias
                    .as_ref()
                    .map(|a| normalize_ident(&a.name))
                    .unwrap_or_else(|| obj_name.clone());

                // Check CTE first (CTEs are always unqualified in PostgreSQL).
                if schema_opt.is_none() {
                    if let Some(cte_cols) = self.scopes.resolve_cte(&obj_name) {
                        let columns: Vec<(String, DataType, bool)> = cte_cols
                            .iter()
                            .map(|(n, dt)| (n.clone(), dt.clone(), true))
                            .collect();

                        self.scopes.current_mut().add_table(&alias_str, &columns);

                        let schema = TableRefSchema {
                            table_id: 0,
                            columns,
                        };

                        return Ok(AnalyzedTableRef {
                            kind: AnalyzedTableRefKind::Table {
                                name: obj_name,
                                schema,
                            },
                            alias: Some(alias_str),
                        });
                    }
                }

                // Try catalog with proper schema qualification.
                match self.catalog.resolve_table(&obj_name, schema_opt.as_deref()) {
                    Ok(Some((qualified_name, table_schema))) => {
                        let columns: Vec<(String, DataType, bool)> = table_schema
                            .columns
                            .iter()
                            .map(|c| (c.name.clone(), c.data_type.clone(), c.nullable))
                            .collect();

                        self.scopes.current_mut().add_table(&alias_str, &columns);

                        let schema = TableRefSchema {
                            table_id: table_schema.table_id,
                            columns,
                        };

                        Ok(AnalyzedTableRef {
                            kind: AnalyzedTableRefKind::Table {
                                name: qualified_name,
                                schema,
                            },
                            alias: Some(alias_str),
                        })
                    }
                    Ok(None) => Err(AnalyzerError::TableNotFound(obj_name)),
                    Err(e) => Err(AnalyzerError::Internal(e.to_string())),
                }
            }

            TableFactor::Derived {
                subquery, alias, ..
            } => {
                let analyzed = self.analyze_query(subquery)?;

                let alias_str = alias
                    .as_ref()
                    .map(|a| normalize_ident(&a.name))
                    .unwrap_or_else(|| "subquery".to_string());

                // Add subquery output columns to current scope
                let mut columns: Vec<(String, DataType, bool)> = analyzed
                    .output_schema
                    .iter()
                    .map(|(name, dt)| (name.clone(), dt.clone(), true))
                    .collect();

                if let Some(a) = alias {
                    if !a.columns.is_empty() {
                        if a.columns.len() != columns.len() {
                            return Err(AnalyzerError::Unsupported(format!(
                                "derived table alias column count mismatch: expected {}, got {}",
                                columns.len(),
                                a.columns.len()
                            )));
                        }
                        for (i, ident) in a.columns.iter().enumerate() {
                            columns[i].0 = normalize_ident(ident);
                        }
                    }
                }
                self.scopes.current_mut().add_table(&alias_str, &columns);

                Ok(AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Subquery(Box::new(analyzed)),
                    alias: Some(alias_str),
                })
            }

            TableFactor::NestedJoin {
                table_with_joins,
                alias,
            } => {
                let mut result = self.analyze_table_with_joins(table_with_joins)?;
                if let Some(a) = alias {
                    result.alias = Some(normalize_ident(&a.name));
                }
                Ok(result)
            }

            other => Err(AnalyzerError::Unsupported(format!(
                "table factor: {:?}",
                std::mem::discriminant(other),
            ))),
        }
    }

    // ── JOIN constraint ─────────────────────────────────────

    fn analyze_join_constraint(
        &mut self,
        join_op: &ast::JoinOperator,
        left_start: usize,
        right_start: usize,
    ) -> Result<(JoinType, JoinCondition), AnalyzerError> {
        match join_op {
            ast::JoinOperator::Inner(constraint) => {
                let cond = self.analyze_join_condition(constraint, left_start, right_start)?;
                Ok((JoinType::Inner, cond))
            }
            ast::JoinOperator::LeftOuter(constraint) => {
                let cond = self.analyze_join_condition(constraint, left_start, right_start)?;
                Ok((JoinType::Left, cond))
            }
            ast::JoinOperator::RightOuter(constraint) => {
                let cond = self.analyze_join_condition(constraint, left_start, right_start)?;
                Ok((JoinType::Right, cond))
            }
            ast::JoinOperator::FullOuter(constraint) => {
                let cond = self.analyze_join_condition(constraint, left_start, right_start)?;
                Ok((JoinType::Full, cond))
            }
            ast::JoinOperator::CrossJoin => Ok((JoinType::Cross, JoinCondition::None)),
            other => Err(AnalyzerError::Unsupported(format!(
                "join type: {:?}",
                std::mem::discriminant(other),
            ))),
        }
    }

    fn analyze_join_condition(
        &mut self,
        constraint: &ast::JoinConstraint,
        left_start: usize,
        right_start: usize,
    ) -> Result<JoinCondition, AnalyzerError> {
        match constraint {
            ast::JoinConstraint::On(expr) => {
                let analyzed = self.analyze_expr(expr)?;
                let analyzed = self.ensure_boolean(analyzed, "JOIN ON clause")?;
                Ok(JoinCondition::On(analyzed))
            }
            ast::JoinConstraint::Using(columns) => {
                let mut resolved = Vec::new();
                for col_ident in columns {
                    let col_name = &col_ident.value;
                    let scope = self.scopes.current();
                    let lower = col_name.to_lowercase();

                    // Find matching column in left side (indices in left_start..right_start).
                    let left_match = scope.columns().iter().find(|c| {
                        c.column_index >= left_start
                            && c.column_index < right_start
                            && c.column_name.to_lowercase() == lower
                    });

                    // Find matching column in right side (indices >= right_start).
                    let right_match = scope.columns().iter().find(|c| {
                        c.column_index >= right_start && c.column_name.to_lowercase() == lower
                    });

                    let (left_col, right_col) = match (left_match, right_match) {
                        (Some(l), Some(r)) => (l, r),
                        _ => {
                            return Err(AnalyzerError::ColumnNotFound {
                                name: col_name.clone(),
                                available: scope.available_columns(),
                            });
                        }
                    };

                    // Unify left and right column types (e.g., INT vs BIGINT).
                    let unified_type = common_type(&left_col.data_type, &right_col.data_type)
                        .ok_or_else(|| AnalyzerError::OperatorTypeMismatch {
                            operator: "=".to_string(),
                            left: left_col.data_type.to_string().to_lowercase(),
                            right: right_col.data_type.to_string().to_lowercase(),
                        })?;

                    resolved.push(ResolvedUsingColumn {
                        name: col_name.clone(),
                        left_index: left_col.column_index - left_start,
                        right_index: right_col.column_index - right_start,
                        data_type: unified_type,
                        left_type: left_col.data_type.clone(),
                        right_type: right_col.data_type.clone(),
                    });
                }
                Ok(JoinCondition::Using(resolved))
            }
            ast::JoinConstraint::Natural => {
                // NATURAL JOIN = USING on all columns with matching names.
                let scope = self.scopes.current();
                let mut resolved = Vec::new();
                let mut seen = std::collections::HashSet::new();

                // Collect left-side column names (only this join's left table).
                let left_cols: Vec<_> = scope
                    .columns()
                    .iter()
                    .filter(|c| c.column_index >= left_start && c.column_index < right_start)
                    .collect();

                // For each left column, find a matching right column.
                for lc in &left_cols {
                    let lower = lc.column_name.to_lowercase();
                    if seen.contains(&lower) {
                        continue;
                    }
                    if let Some(rc) = scope.columns().iter().find(|c| {
                        c.column_index >= right_start && c.column_name.to_lowercase() == lower
                    }) {
                        let unified_type =
                            common_type(&lc.data_type, &rc.data_type).ok_or_else(|| {
                                AnalyzerError::OperatorTypeMismatch {
                                    operator: "=".to_string(),
                                    left: lc.data_type.to_string().to_lowercase(),
                                    right: rc.data_type.to_string().to_lowercase(),
                                }
                            })?;
                        resolved.push(ResolvedUsingColumn {
                            name: lc.column_name.clone(),
                            left_index: lc.column_index - left_start,
                            right_index: rc.column_index - right_start,
                            data_type: unified_type,
                            left_type: lc.data_type.clone(),
                            right_type: rc.data_type.clone(),
                        });
                        seen.insert(lower);
                    }
                }

                Ok(JoinCondition::Using(resolved))
            }
            ast::JoinConstraint::None => Ok(JoinCondition::None),
        }
    }

    // ── Set operation validation ─────────────────────────────

    /// Validate and unify schemas across set operation branches.
    ///
    /// Checks that left and right have the same column count, then unifies
    /// corresponding column types (e.g. Int32 + Int64 → Int64).
    fn unify_set_operation_schemas(
        &self,
        left: &AnalyzedQuery,
        right: &AnalyzedQuery,
        op: SetOpKind,
    ) -> Result<Vec<(String, DataType)>, AnalyzerError> {
        if left.output_schema.len() != right.output_schema.len() {
            return Err(AnalyzerError::SetOperationColumnMismatch {
                left: left.output_schema.len(),
                right: right.output_schema.len(),
            });
        }

        let op_name = match op {
            SetOpKind::Union => "UNION",
            SetOpKind::Intersect => "INTERSECT",
            SetOpKind::Except => "EXCEPT",
        };

        left.output_schema
            .iter()
            .zip(right.output_schema.iter())
            .map(|((name, left_dt), (_, right_dt))| {
                if left_dt == right_dt {
                    Ok((name.clone(), left_dt.clone()))
                } else {
                    let unified = common_type(left_dt, right_dt).ok_or_else(|| {
                        AnalyzerError::TypesCannotBeMatched {
                            types: vec![left_dt.clone(), right_dt.clone()],
                            context: op_name.to_string(),
                        }
                    })?;
                    Ok((name.clone(), unified))
                }
            })
            .collect()
    }

    /// Wrap a set operation arm in a coercing projection if needed.
    ///
    /// For each output column, if the arm's type differs from the unified output
    /// type, we wrap the arm as:
    ///
    /// ```sql
    /// SELECT CAST(col_i AS unified_i) AS name_i, ...
    /// FROM (<arm>) AS <subquery_alias>
    /// ```
    ///
    /// This preserves the arm's own ORDER BY/LIMIT/OFFSET semantics.
    fn wrap_set_op_arm_with_coercion(
        &self,
        arm: AnalyzedQuery,
        unified_schema: &[(String, DataType)],
        subquery_alias: &str,
    ) -> AnalyzedQuery {
        let arm_output_schema = arm.output_schema.clone();
        let needs_wrap = arm
            .output_schema
            .iter()
            .zip(unified_schema.iter())
            .any(|((_, arm_ty), (_, unified_ty))| arm_ty != unified_ty);

        if !needs_wrap {
            return arm;
        }

        let from = vec![AnalyzedTableRef {
            kind: AnalyzedTableRefKind::Subquery(Box::new(arm)),
            alias: Some(subquery_alias.to_string()),
        }];

        let projection: Vec<AnalyzedProjection> = unified_schema
            .iter()
            .enumerate()
            .map(|(idx, (out_name, unified_ty))| {
                // The input type is the arm's output type at this position.
                let (input_name, input_ty) = &arm_output_schema[idx];

                let col_ref = TypedExpr::new(
                    TypedExprKind::ColumnRef {
                        scope_depth: 0,
                        column_index: idx,
                        column_name: input_name.clone(),
                    },
                    input_ty.clone(),
                );

                let expr = if col_ref.data_type == *unified_ty {
                    col_ref
                } else {
                    TypedExpr::new(
                        TypedExprKind::Cast {
                            expr: Box::new(col_ref),
                            target_type: unified_ty.clone(),
                            cast_context: CastContext::Implicit,
                        },
                        unified_ty.clone(),
                    )
                };

                AnalyzedProjection {
                    expr,
                    output_name: out_name.clone(),
                }
            })
            .collect();

        AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection,
                from,
                where_clause: None,
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: unified_schema.to_vec(),
        }
    }

    // ── GROUP BY ────────────────────────────────────────────

    fn analyze_group_by(
        &mut self,
        group_by: &ast::GroupByExpr,
        select_items: &[SelectItem],
    ) -> Result<Vec<TypedExpr>, AnalyzerError> {
        match group_by {
            ast::GroupByExpr::All => Err(AnalyzerError::Unsupported("GROUP BY ALL".to_string())),
            ast::GroupByExpr::Expressions(exprs) => exprs
                .iter()
                .map(|e| {
                    // Resolve positional references: GROUP BY 1 → SELECT item at position 1.
                    if let Expr::Value(ast::Value::Number(n, _)) = e {
                        if let Ok(pos) = n.parse::<usize>() {
                            if pos >= 1 && pos <= select_items.len() {
                                let ast_expr = match &select_items[pos - 1] {
                                    SelectItem::UnnamedExpr(expr) => expr,
                                    SelectItem::ExprWithAlias { expr, .. } => expr,
                                    _ => return self.analyze_expr(e),
                                };
                                return self.analyze_expr(ast_expr);
                            }
                        }
                    }
                    // Resolve alias references: GROUP BY alias → SELECT expr with that alias.
                    // PostgreSQL allows GROUP BY to reference SELECT output aliases.
                    if let Expr::Identifier(ident) = e {
                        let lookup = normalize_ident(ident);
                        let is_quoted = ident.quote_style.is_some();
                        for item in select_items {
                            if let SelectItem::ExprWithAlias { expr, alias } = item {
                                let alias_name = normalize_ident(alias);
                                if (is_quoted && alias_name == lookup)
                                    || (!is_quoted && alias_name.to_lowercase() == lookup)
                                {
                                    return self.analyze_expr(expr);
                                }
                            }
                        }
                    }
                    self.analyze_expr(e)
                })
                .collect(),
        }
    }

    /// Validate GROUP BY / aggregate query semantics for SELECT and HAVING.
    ///
    /// In grouped or aggregate queries, every current-scope column reference
    /// outside aggregate calls must be covered by GROUP BY.
    fn validate_grouping_semantics(
        &self,
        group_by: &[TypedExpr],
        projection: &[AnalyzedProjection],
        having: Option<&TypedExpr>,
        order_by: &[TypedOrderByExpr],
    ) -> Result<(), AnalyzerError> {
        let has_projection_aggregate = projection
            .iter()
            .any(|p| Self::contains_aggregate_call(&p.expr));
        let is_grouped_query = !group_by.is_empty() || has_projection_aggregate || having.is_some();

        if !is_grouped_query {
            return Ok(());
        }

        let grouped_expr_keys: HashSet<String> =
            group_by.iter().map(|e| format!("{}", e)).collect();
        let grouped_columns: HashSet<usize> = group_by
            .iter()
            .filter_map(|e| match &e.kind {
                TypedExprKind::ColumnRef {
                    scope_depth,
                    column_index,
                    ..
                } if *scope_depth == 0 => Some(*column_index),
                _ => None,
            })
            .collect();

        // PG qualifies ungrouped column names with their table alias.
        let scope_cols = self.scopes.current().columns().to_vec();
        let qualify = |name: String| -> String {
            scope_cols
                .iter()
                .find(|c| c.column_name == name)
                .and_then(|c| c.table_alias.as_ref())
                .map(|t| format!("{}.{}", t, name))
                .unwrap_or(name)
        };

        for p in projection {
            if let Some(name) =
                Self::find_ungrouped_column(&p.expr, &grouped_columns, &grouped_expr_keys, false)
            {
                return Err(AnalyzerError::UngroupedColumn {
                    name: qualify(name),
                });
            }
        }

        if let Some(having_expr) = having {
            if let Some(name) = Self::find_ungrouped_column(
                having_expr,
                &grouped_columns,
                &grouped_expr_keys,
                false,
            ) {
                return Err(AnalyzerError::UngroupedColumn {
                    name: qualify(name),
                });
            }
        }

        for ob in order_by {
            if let Some(name) =
                Self::find_ungrouped_column(&ob.expr, &grouped_columns, &grouped_expr_keys, false)
            {
                return Err(AnalyzerError::UngroupedColumn {
                    name: qualify(name),
                });
            }
        }

        Ok(())
    }

    fn contains_aggregate_call(expr: &TypedExpr) -> bool {
        match &expr.kind {
            TypedExprKind::AggregateCall { .. } => true,
            TypedExprKind::BinaryOp { left, right, .. } => {
                Self::contains_aggregate_call(left) || Self::contains_aggregate_call(right)
            }
            TypedExprKind::UnaryOp { operand, .. }
            | TypedExprKind::Cast { expr: operand, .. }
            | TypedExprKind::IsTest { expr: operand, .. } => Self::contains_aggregate_call(operand),
            TypedExprKind::Between {
                expr, low, high, ..
            } => {
                Self::contains_aggregate_call(expr)
                    || Self::contains_aggregate_call(low)
                    || Self::contains_aggregate_call(high)
            }
            TypedExprKind::InList { expr, list, .. } => {
                Self::contains_aggregate_call(expr)
                    || list.iter().any(Self::contains_aggregate_call)
            }
            TypedExprKind::Like {
                expr,
                pattern,
                escape,
                ..
            }
            | TypedExprKind::SimilarTo {
                expr,
                pattern,
                escape,
                ..
            } => {
                Self::contains_aggregate_call(expr)
                    || Self::contains_aggregate_call(pattern)
                    || escape
                        .as_ref()
                        .is_some_and(|e| Self::contains_aggregate_call(e))
            }
            TypedExprKind::Case {
                operand,
                when_clauses,
                else_result,
            } => {
                operand
                    .as_ref()
                    .is_some_and(|e| Self::contains_aggregate_call(e))
                    || when_clauses.iter().any(|(w, t)| {
                        Self::contains_aggregate_call(w) || Self::contains_aggregate_call(t)
                    })
                    || else_result
                        .as_ref()
                        .is_some_and(|e| Self::contains_aggregate_call(e))
            }
            TypedExprKind::Coalesce(args)
            | TypedExprKind::MinMax { args, .. }
            | TypedExprKind::ArrayLiteral(args)
            | TypedExprKind::Row(args) => args.iter().any(Self::contains_aggregate_call),
            TypedExprKind::NullIf(a, b) => {
                Self::contains_aggregate_call(a) || Self::contains_aggregate_call(b)
            }
            TypedExprKind::FunctionCall {
                args,
                order_by,
                filter,
                ..
            } => {
                args.iter().any(Self::contains_aggregate_call)
                    || order_by
                        .iter()
                        .any(|o| Self::contains_aggregate_call(&o.expr))
                    || filter
                        .as_ref()
                        .is_some_and(|f| Self::contains_aggregate_call(f))
            }
            TypedExprKind::WindowCall {
                args,
                partition_by,
                order_by,
                window_frame,
                ..
            } => {
                args.iter().any(Self::contains_aggregate_call)
                    || partition_by.iter().any(Self::contains_aggregate_call)
                    || order_by
                        .iter()
                        .any(|o| Self::contains_aggregate_call(&o.expr))
                    || window_frame.as_ref().is_some_and(|frame| {
                        Self::contains_aggregate_in_window_bound(&frame.start)
                            || frame
                                .end
                                .as_ref()
                                .is_some_and(Self::contains_aggregate_in_window_bound)
                    })
            }
            TypedExprKind::InSubquery { expr, .. } | TypedExprKind::AnyAll { expr, .. } => {
                Self::contains_aggregate_call(expr)
            }
            TypedExprKind::ArrayIndex { array, index } => {
                Self::contains_aggregate_call(array) || Self::contains_aggregate_call(index)
            }
            TypedExprKind::JsonAccess { expr, path, .. } => {
                Self::contains_aggregate_call(expr) || Self::contains_aggregate_call(path)
            }
            TypedExprKind::Constant(_)
            | TypedExprKind::ColumnRef { .. }
            | TypedExprKind::ScalarSubquery(_)
            | TypedExprKind::ArraySubquery(_)
            | TypedExprKind::Exists { .. }
            | TypedExprKind::Default
            | TypedExprKind::Parameter { .. } => false,
        }
    }

    fn contains_aggregate_in_window_bound(bound: &WindowFrameBound) -> bool {
        match bound {
            WindowFrameBound::CurrentRow => false,
            WindowFrameBound::Preceding(expr) | WindowFrameBound::Following(expr) => expr
                .as_ref()
                .is_some_and(|e| Self::contains_aggregate_call(e)),
        }
    }

    fn find_ungrouped_column(
        expr: &TypedExpr,
        grouped_columns: &HashSet<usize>,
        grouped_expr_keys: &HashSet<String>,
        in_aggregate: bool,
    ) -> Option<String> {
        if !in_aggregate && grouped_expr_keys.contains(&format!("{}", expr)) {
            return None;
        }

        match &expr.kind {
            TypedExprKind::ColumnRef {
                scope_depth,
                column_index,
                column_name,
            } => {
                if !in_aggregate && *scope_depth == 0 && !grouped_columns.contains(column_index) {
                    Some(column_name.clone())
                } else {
                    None
                }
            }

            // Aggregate call establishes a new scope where columns are legal.
            TypedExprKind::AggregateCall { .. } => None,

            TypedExprKind::BinaryOp { left, right, .. } => {
                Self::find_ungrouped_column(left, grouped_columns, grouped_expr_keys, in_aggregate)
                    .or_else(|| {
                        Self::find_ungrouped_column(
                            right,
                            grouped_columns,
                            grouped_expr_keys,
                            in_aggregate,
                        )
                    })
            }

            TypedExprKind::UnaryOp { operand, .. }
            | TypedExprKind::Cast { expr: operand, .. }
            | TypedExprKind::IsTest { expr: operand, .. } => Self::find_ungrouped_column(
                operand,
                grouped_columns,
                grouped_expr_keys,
                in_aggregate,
            ),

            TypedExprKind::Between {
                expr, low, high, ..
            } => {
                Self::find_ungrouped_column(expr, grouped_columns, grouped_expr_keys, in_aggregate)
                    .or_else(|| {
                        Self::find_ungrouped_column(
                            low,
                            grouped_columns,
                            grouped_expr_keys,
                            in_aggregate,
                        )
                    })
                    .or_else(|| {
                        Self::find_ungrouped_column(
                            high,
                            grouped_columns,
                            grouped_expr_keys,
                            in_aggregate,
                        )
                    })
            }

            TypedExprKind::InList { expr, list, .. } => {
                Self::find_ungrouped_column(expr, grouped_columns, grouped_expr_keys, in_aggregate)
                    .or_else(|| {
                        list.iter().find_map(|e| {
                            Self::find_ungrouped_column(
                                e,
                                grouped_columns,
                                grouped_expr_keys,
                                in_aggregate,
                            )
                        })
                    })
            }

            TypedExprKind::Like {
                expr,
                pattern,
                escape,
                ..
            }
            | TypedExprKind::SimilarTo {
                expr,
                pattern,
                escape,
                ..
            } => {
                Self::find_ungrouped_column(expr, grouped_columns, grouped_expr_keys, in_aggregate)
                    .or_else(|| {
                        Self::find_ungrouped_column(
                            pattern,
                            grouped_columns,
                            grouped_expr_keys,
                            in_aggregate,
                        )
                    })
                    .or_else(|| {
                        escape.as_ref().and_then(|e| {
                            Self::find_ungrouped_column(
                                e,
                                grouped_columns,
                                grouped_expr_keys,
                                in_aggregate,
                            )
                        })
                    })
            }

            TypedExprKind::Case {
                operand,
                when_clauses,
                else_result,
            } => operand
                .as_ref()
                .and_then(|e| {
                    Self::find_ungrouped_column(e, grouped_columns, grouped_expr_keys, in_aggregate)
                })
                .or_else(|| {
                    when_clauses.iter().find_map(|(w, t)| {
                        Self::find_ungrouped_column(
                            w,
                            grouped_columns,
                            grouped_expr_keys,
                            in_aggregate,
                        )
                        .or_else(|| {
                            Self::find_ungrouped_column(
                                t,
                                grouped_columns,
                                grouped_expr_keys,
                                in_aggregate,
                            )
                        })
                    })
                })
                .or_else(|| {
                    else_result.as_ref().and_then(|e| {
                        Self::find_ungrouped_column(
                            e,
                            grouped_columns,
                            grouped_expr_keys,
                            in_aggregate,
                        )
                    })
                }),

            TypedExprKind::Coalesce(args)
            | TypedExprKind::MinMax { args, .. }
            | TypedExprKind::ArrayLiteral(args)
            | TypedExprKind::Row(args) => args.iter().find_map(|e| {
                Self::find_ungrouped_column(e, grouped_columns, grouped_expr_keys, in_aggregate)
            }),

            TypedExprKind::NullIf(a, b) => {
                Self::find_ungrouped_column(a, grouped_columns, grouped_expr_keys, in_aggregate)
                    .or_else(|| {
                        Self::find_ungrouped_column(
                            b,
                            grouped_columns,
                            grouped_expr_keys,
                            in_aggregate,
                        )
                    })
            }

            TypedExprKind::FunctionCall {
                args,
                order_by,
                filter,
                ..
            } => args
                .iter()
                .find_map(|e| {
                    Self::find_ungrouped_column(e, grouped_columns, grouped_expr_keys, in_aggregate)
                })
                .or_else(|| {
                    order_by.iter().find_map(|o| {
                        Self::find_ungrouped_column(
                            &o.expr,
                            grouped_columns,
                            grouped_expr_keys,
                            in_aggregate,
                        )
                    })
                })
                .or_else(|| {
                    filter.as_ref().and_then(|e| {
                        Self::find_ungrouped_column(
                            e,
                            grouped_columns,
                            grouped_expr_keys,
                            in_aggregate,
                        )
                    })
                }),

            TypedExprKind::WindowCall {
                args,
                partition_by,
                order_by,
                window_frame,
                ..
            } => args
                .iter()
                .find_map(|e| {
                    Self::find_ungrouped_column(e, grouped_columns, grouped_expr_keys, in_aggregate)
                })
                .or_else(|| {
                    partition_by.iter().find_map(|e| {
                        Self::find_ungrouped_column(
                            e,
                            grouped_columns,
                            grouped_expr_keys,
                            in_aggregate,
                        )
                    })
                })
                .or_else(|| {
                    order_by.iter().find_map(|o| {
                        Self::find_ungrouped_column(
                            &o.expr,
                            grouped_columns,
                            grouped_expr_keys,
                            in_aggregate,
                        )
                    })
                })
                .or_else(|| {
                    window_frame.as_ref().and_then(|frame| {
                        Self::find_ungrouped_in_window_bound(
                            &frame.start,
                            grouped_columns,
                            grouped_expr_keys,
                            in_aggregate,
                        )
                        .or_else(|| {
                            frame.end.as_ref().and_then(|b| {
                                Self::find_ungrouped_in_window_bound(
                                    b,
                                    grouped_columns,
                                    grouped_expr_keys,
                                    in_aggregate,
                                )
                            })
                        })
                    })
                }),

            TypedExprKind::InSubquery { expr, .. } | TypedExprKind::AnyAll { expr, .. } => {
                Self::find_ungrouped_column(expr, grouped_columns, grouped_expr_keys, in_aggregate)
            }

            TypedExprKind::ArrayIndex { array, index } => {
                Self::find_ungrouped_column(array, grouped_columns, grouped_expr_keys, in_aggregate)
                    .or_else(|| {
                        Self::find_ungrouped_column(
                            index,
                            grouped_columns,
                            grouped_expr_keys,
                            in_aggregate,
                        )
                    })
            }

            TypedExprKind::JsonAccess { expr, path, .. } => {
                Self::find_ungrouped_column(expr, grouped_columns, grouped_expr_keys, in_aggregate)
                    .or_else(|| {
                        Self::find_ungrouped_column(
                            path,
                            grouped_columns,
                            grouped_expr_keys,
                            in_aggregate,
                        )
                    })
            }

            TypedExprKind::Constant(_)
            | TypedExprKind::ScalarSubquery(_)
            | TypedExprKind::ArraySubquery(_)
            | TypedExprKind::Exists { .. }
            | TypedExprKind::Default
            | TypedExprKind::Parameter { .. } => None,
        }
    }

    fn find_ungrouped_in_window_bound(
        bound: &WindowFrameBound,
        grouped_columns: &HashSet<usize>,
        grouped_expr_keys: &HashSet<String>,
        in_aggregate: bool,
    ) -> Option<String> {
        match bound {
            WindowFrameBound::CurrentRow => None,
            WindowFrameBound::Preceding(expr) | WindowFrameBound::Following(expr) => {
                expr.as_ref().and_then(|e| {
                    Self::find_ungrouped_column(e, grouped_columns, grouped_expr_keys, in_aggregate)
                })
            }
        }
    }

    // ── SELECT projection ───────────────────────────────────

    fn build_wildcard_projection_order(
        select: &Select,
        from_refs: &[AnalyzedTableRef],
    ) -> Option<Vec<(String, usize)>> {
        let mut sources: Vec<TableSchema> = Vec::new();
        for table_ref in from_refs {
            Self::collect_wildcard_sources(table_ref, &mut sources);
        }

        let schema_refs: Vec<&TableSchema> = sources.iter().collect();
        let plan = crate::sql::wildcard::build_join_wildcard_plan(select, &schema_refs)?;
        if !plan.any_merge {
            return None;
        }

        let mut source_offsets = Vec::with_capacity(sources.len());
        let mut next_offset = 0usize;
        for source in &sources {
            source_offsets.push(next_offset);
            next_offset = next_offset.saturating_add(source.columns.len());
        }

        let mut ordered = Vec::with_capacity(plan.columns.len());
        for col in plan.columns {
            let source_offset = *source_offsets.get(col.source_idx)?;
            ordered.push((col.name, source_offset + col.col_idx));
        }
        Some(ordered)
    }

    fn collect_wildcard_sources(table_ref: &AnalyzedTableRef, out: &mut Vec<TableSchema>) {
        match &table_ref.kind {
            AnalyzedTableRefKind::Join { left, right, .. } => {
                Self::collect_wildcard_sources(left, out);
                Self::collect_wildcard_sources(right, out);
            }
            _ => out.push(Self::leaf_wildcard_source_schema(table_ref)),
        }
    }

    fn leaf_wildcard_source_schema(table_ref: &AnalyzedTableRef) -> TableSchema {
        match &table_ref.kind {
            AnalyzedTableRefKind::Table { name, schema } => TableSchema {
                name: name.clone(),
                table_id: 0,
                columns: schema
                    .columns
                    .iter()
                    .map(|(col_name, data_type, nullable)| ColumnDef {
                        name: col_name.clone(),
                        data_type: data_type.clone(),
                        nullable: *nullable,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    })
                    .collect(),
                version: 1,
                pk_constraint_name: None,
                pk_indices: vec![],
                indexes: vec![],
                check_constraints: vec![],
                foreign_keys: vec![],
                owner: String::new(),
                from_alias: None,
            },
            AnalyzedTableRefKind::Subquery(query) => TableSchema {
                name: "subquery".to_string(),
                table_id: 0,
                columns: query
                    .output_schema
                    .iter()
                    .map(|(col_name, data_type)| ColumnDef {
                        name: col_name.clone(),
                        data_type: data_type.clone(),
                        nullable: true,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    })
                    .collect(),
                version: 1,
                pk_constraint_name: None,
                pk_indices: vec![],
                indexes: vec![],
                check_constraints: vec![],
                foreign_keys: vec![],
                owner: String::new(),
                from_alias: None,
            },
            AnalyzedTableRefKind::Function {
                func,
                output_columns,
                ..
            } => TableSchema {
                name: func.name.clone(),
                table_id: 0,
                columns: output_columns
                    .iter()
                    .map(|(col_name, data_type)| ColumnDef {
                        name: col_name.clone(),
                        data_type: data_type.clone(),
                        nullable: true,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    })
                    .collect(),
                version: 1,
                pk_constraint_name: None,
                pk_indices: vec![],
                indexes: vec![],
                check_constraints: vec![],
                foreign_keys: vec![],
                owner: String::new(),
                from_alias: None,
            },
            AnalyzedTableRefKind::Join { .. } => unreachable!(),
        }
    }

    fn cast_to_implicit(expr: TypedExpr, target_type: &DataType) -> TypedExpr {
        if expr.data_type == *target_type {
            expr
        } else {
            TypedExpr::new(
                TypedExprKind::Cast {
                    expr: Box::new(expr),
                    target_type: target_type.clone(),
                    cast_context: CastContext::Implicit,
                },
                target_type.clone(),
            )
        }
    }

    fn scope_column_projection(
        scope: &Scope,
        column_index: usize,
    ) -> Option<(String, TypedExpr, DataType)> {
        let col = scope.columns().get(column_index)?;
        if let Some(merged) = scope.using_column_for_left(column_index) {
            let left_expr = TypedExpr::new(
                TypedExprKind::ColumnRef {
                    scope_depth: 0,
                    column_index: merged.left_index,
                    column_name: merged.column_name.clone(),
                },
                merged.left_type.clone(),
            );
            let right_expr = TypedExpr::new(
                TypedExprKind::ColumnRef {
                    scope_depth: 0,
                    column_index: merged.right_index,
                    column_name: merged.column_name.clone(),
                },
                merged.right_type.clone(),
            );

            let expr = TypedExpr::new(
                TypedExprKind::Coalesce(vec![
                    Self::cast_to_implicit(left_expr, &merged.data_type),
                    Self::cast_to_implicit(right_expr, &merged.data_type),
                ]),
                merged.data_type.clone(),
            );
            return Some((col.column_name.clone(), expr, merged.data_type.clone()));
        }

        let expr = TypedExpr::new(
            TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: col.column_index,
                column_name: col.column_name.clone(),
            },
            col.data_type.clone(),
        );
        Some((col.column_name.clone(), expr, col.data_type.clone()))
    }

    pub(super) fn analyze_projection(
        &mut self,
        items: &[SelectItem],
        wildcard_order: Option<&[(String, usize)]>,
    ) -> Result<(Vec<AnalyzedProjection>, Vec<(String, DataType)>), AnalyzerError> {
        let mut projection = Vec::new();
        let mut output_schema = Vec::new();

        for item in items {
            match item {
                SelectItem::UnnamedExpr(expr) => {
                    let analyzed = self.analyze_expr(expr)?;
                    let name = self.infer_column_alias(expr);
                    output_schema.push((name.clone(), analyzed.data_type.clone()));
                    projection.push(AnalyzedProjection {
                        expr: analyzed,
                        output_name: name,
                    });
                }

                SelectItem::ExprWithAlias { expr, alias } => {
                    let analyzed = self.analyze_expr(expr)?;
                    let name = alias.value.clone();
                    output_schema.push((name.clone(), analyzed.data_type.clone()));
                    projection.push(AnalyzedProjection {
                        expr: analyzed,
                        output_name: name,
                    });
                }

                SelectItem::Wildcard(_) => {
                    let scope = self.scopes.current();
                    if let Some(order) = wildcard_order {
                        for (name, column_index) in order {
                            if let Some((_col_name, expr, data_type)) =
                                Self::scope_column_projection(scope, *column_index)
                            {
                                output_schema.push((name.clone(), data_type.clone()));
                                projection.push(AnalyzedProjection {
                                    expr,
                                    output_name: name.clone(),
                                });
                            }
                        }
                        continue;
                    }

                    for col in scope.columns() {
                        // Skip USING join right-side duplicate columns.
                        if col.hidden {
                            continue;
                        }
                        if let Some((name, expr, data_type)) =
                            Self::scope_column_projection(scope, col.column_index)
                        {
                            output_schema.push((name.clone(), data_type.clone()));
                            projection.push(AnalyzedProjection {
                                expr,
                                output_name: name,
                            });
                        }
                    }
                }

                SelectItem::QualifiedWildcard(name, _) => {
                    let table_name = name.to_string();
                    let scope = self.scopes.current();
                    let lower_table = table_name.to_lowercase();

                    let matching_cols: Vec<_> = scope
                        .columns()
                        .iter()
                        .filter(|c| {
                            c.table_alias
                                .as_ref()
                                .map(|a| a.to_lowercase() == lower_table)
                                .unwrap_or(false)
                        })
                        .collect();

                    if matching_cols.is_empty() {
                        return Err(AnalyzerError::ColumnNotFound {
                            name: format!("{}.*", table_name),
                            available: self.scopes.current().available_columns(),
                        });
                    }

                    for col in matching_cols {
                        let expr = TypedExpr::new(
                            TypedExprKind::ColumnRef {
                                scope_depth: 0,
                                column_index: col.column_index,
                                column_name: col.column_name.clone(),
                            },
                            col.data_type.clone(),
                        );
                        output_schema.push((col.column_name.clone(), col.data_type.clone()));
                        projection.push(AnalyzedProjection {
                            expr,
                            output_name: col.column_name.clone(),
                        });
                    }
                }
            }
        }

        Ok((projection, output_schema))
    }

    // ── DISTINCT ─────────────────────────────────────────────

    fn analyze_distinct(
        &mut self,
        distinct: &Option<ast::Distinct>,
    ) -> Result<AnalyzedDistinct, AnalyzerError> {
        match distinct {
            None => Ok(AnalyzedDistinct::All),
            Some(ast::Distinct::Distinct) => Ok(AnalyzedDistinct::Distinct),
            Some(ast::Distinct::On(exprs)) => {
                let analyzed: Vec<TypedExpr> = exprs
                    .iter()
                    .map(|e| self.analyze_expr(e))
                    .collect::<Result<_, _>>()?;
                Ok(AnalyzedDistinct::DistinctOn(analyzed))
            }
        }
    }

    /// Infer a column alias from an expression (for unaliased SELECT items).
    fn infer_column_alias(&self, expr: &Expr) -> String {
        fn infer(expr: &Expr) -> Option<String> {
            match expr {
                Expr::Identifier(ident) => Some(ident.value.clone()),
                Expr::CompoundIdentifier(parts) => parts.last().map(|i| i.value.clone()),
                Expr::Function(func) => {
                    Some(crate::sql::names::function_name_upper(func).to_lowercase())
                }
                Expr::ArrayAgg(_) => Some("array_agg".to_string()),
                Expr::ListAgg(_) => Some("listagg".to_string()),
                Expr::Nested(inner) => infer(inner),
                Expr::Array(_) | Expr::ArrayIndex { .. } => Some("array".to_string()),
                Expr::ArraySubquery(_) => Some("array".to_string()),
                Expr::Case {
                    results,
                    else_result,
                    ..
                } => {
                    if let Some(inner) = else_result {
                        if let Some(name) = infer(inner) {
                            return Some(name);
                        }
                    }
                    for result in results {
                        if let Some(name) = infer(result) {
                            return Some(name);
                        }
                    }
                    Some("case".to_string())
                }
                _ => None,
            }
        }

        infer(expr).unwrap_or_else(|| "?column?".to_string())
    }

    // ── CTE analysis ────────────────────────────────────────

    /// Analyze CTE definitions from a WITH clause.
    ///
    /// Registers each CTE in the current scope so it can be resolved as a table
    /// in FROM clauses. Each CTE can reference previously-defined CTEs in the
    /// same WITH clause (non-recursive).
    fn analyze_cte_definitions(
        &mut self,
        with: Option<&ast::With>,
    ) -> Result<Vec<AnalyzedCte>, AnalyzerError> {
        let mut ctes = Vec::new();

        if let Some(with) = with {
            for cte in &with.cte_tables {
                let cte_name = cte.alias.name.value.clone();

                // Analyze CTE query body
                let analyzed = self.analyze_query(&cte.query)?;

                // Determine output columns (may be overridden by explicit column list)
                let columns: Vec<(String, DataType)> = if cte.alias.columns.is_empty() {
                    analyzed.output_schema.clone()
                } else {
                    cte.alias
                        .columns
                        .iter()
                        .zip(analyzed.output_schema.iter())
                        .map(|(alias_col, (_, dt))| (alias_col.value.clone(), dt.clone()))
                        .collect()
                };

                // Register in current scope for subsequent CTEs and main query
                self.scopes
                    .current_mut()
                    .add_cte(&cte_name, columns.clone());

                ctes.push(AnalyzedCte {
                    name: cte_name,
                    query: analyzed,
                    columns,
                    // sqlparser 0.40 does not expose MATERIALIZED/NOT MATERIALIZED;
                    // None = optimizer decides.
                    materialized: None,
                });
            }
        }

        Ok(ctes)
    }
}
