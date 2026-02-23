//! Query-level analysis: FROM, JOIN, WHERE, GROUP BY, HAVING, SELECT, ORDER BY.
//!
//! Builds `AnalyzedQuery` from `sqlparser::ast::Query` by analyzing each clause
//! in dependency order: CTEs -> FROM -> WHERE -> GROUP BY -> HAVING -> SELECT -> ORDER BY.
//!
//! Scope lifecycle: each SELECT pushes one scope that holds both CTE schemas and
//! FROM columns. ORDER BY/LIMIT/OFFSET are analyzed within the same scope so they
//! can reference FROM columns (PostgreSQL semantics).

mod from_clause;
mod group_by;
mod grouping_rewrite;
mod projection;
mod set_expr;

use sqlparser::ast::{self as ast, Expr, Query, Select, SetExpr};

use crate::sql::expr::typed_visit::expr_any;
use crate::sql::types::coercion::common_type;
use crate::sql::types::CastContext;
use crate::types::{DataType, Value};

use super::error::AnalyzerError;
use super::scope::Scope;
use super::types::*;
use super::Analyzer;

/// Extract collation names from an AnalyzedQuery's projection expressions.
///
/// For SELECT bodies, walks each projection expression to find the collation
/// name string from any `Collate` wrapper. For set operations (UNION/INTERSECT/
/// EXCEPT), recurses into the left arm since set operations inherit collation
/// from the left branch. For VALUES, returns `None` for all columns.
fn extract_output_collation_names(query: &AnalyzedQuery) -> Vec<Option<String>> {
    use crate::sql::expr::collation_aware::extract_collation;

    match &query.body {
        AnalyzedQueryBody::Select(sel) => sel
            .projection
            .iter()
            .map(|p| extract_collation(&p.expr))
            .collect(),
        AnalyzedQueryBody::SetOperation { left, .. } => {
            // Set operations inherit collation from the left branch
            // (see unify_set_operation_schemas). Recurse to find the
            // original collation names from the leftmost SELECT arm.
            extract_output_collation_names(left)
        }
        _ => vec![None; query.output_schema.len()],
    }
}

impl<'a> Analyzer<'a> {
    /// Validate that an expression has Boolean type (for WHERE, HAVING, JOIN ON, CASE WHEN).
    ///
    /// The Analyzer contract requires all boolean contexts to be validated at
    /// analysis time. This catches errors like `WHERE text_column` before any
    /// row is touched.
    pub(super) fn ensure_boolean(
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
                    self.wrap_set_op_arm_with_coercion(left_query, &output_schema, "__setop_l")?;
                let right_query =
                    self.wrap_set_op_arm_with_coercion(right_query, &output_schema, "__setop_r")?;

                // Add output columns to scope for ORDER BY resolution.
                // Set operation ORDER BY references output column names, not FROM columns.
                for (name, dt, _coll) in &output_schema {
                    self.scopes
                        .current_mut()
                        .add_column(None, name, dt.clone(), true, None);
                }

                let analyzed_order_by = self.analyze_order_by_exprs(&query.order_by, &[])?;
                let analyzed_limit = self.analyze_limit_expr(&query.limit)?;
                let analyzed_offset = self.analyze_offset_expr(&query.offset)?;

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

    /// Analyze a SELECT with CTEs and ORDER BY/LIMIT/OFFSET -- all within one scope.
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
        // Aggregates and windows start disabled -- only enabled for clauses
        // where they are semantically valid (HAVING, SELECT, ORDER BY).
        self.scopes.push(Scope::new());

        // 1. Register CTEs (visible for FROM table resolution)
        let ctes = self.analyze_cte_definitions(with)?;

        // 2. FROM clause -> populate scope with table columns
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

        // 9/10. LIMIT/OFFSET -- resolve params to Int64
        let analyzed_limit = self.analyze_limit_expr(limit)?;
        let analyzed_offset = self.analyze_offset_expr(offset)?;

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
        for (name, dt, _coll) in &output_schema {
            self.scopes
                .current_mut()
                .add_column(None, name, dt.clone(), true, None);
        }

        // Build synthetic projection so ORDER BY positional refs (ORDER BY 1) map correctly.
        let projection: Vec<AnalyzedProjection> = output_schema
            .iter()
            .enumerate()
            .map(|(idx, (name, dt, _coll))| AnalyzedProjection {
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
        let analyzed_limit = self.analyze_limit_expr(limit)?;
        let analyzed_offset = self.analyze_offset_expr(offset)?;

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

    fn analyze_limit_expr(
        &mut self,
        limit: &Option<ast::Expr>,
    ) -> Result<Option<TypedExpr>, AnalyzerError> {
        match limit {
            Some(expr) => {
                let analyzed = self.analyze_expr(expr)?;
                let analyzed = self.resolve_limit_offset_param_type(analyzed)?;
                self.validate_limit_offset_expr(&analyzed, "LIMIT")?;
                Ok(Some(analyzed))
            }
            None => Ok(None),
        }
    }

    fn analyze_offset_expr(
        &mut self,
        offset: &Option<ast::Offset>,
    ) -> Result<Option<TypedExpr>, AnalyzerError> {
        match offset {
            Some(offset) => {
                let analyzed = self.analyze_expr(&offset.value)?;
                let analyzed = self.resolve_limit_offset_param_type(analyzed)?;
                self.validate_limit_offset_expr(&analyzed, "OFFSET")?;
                Ok(Some(analyzed))
            }
            None => Ok(None),
        }
    }

    fn validate_limit_offset_expr(
        &self,
        expr: &TypedExpr,
        clause: &str,
    ) -> Result<(), AnalyzerError> {
        // PostgreSQL semantics: LIMIT/OFFSET cannot depend on row variables.
        // Parameters are allowed and resolved separately.
        let has_row_variable = expr_any(expr, &|node| {
            matches!(node.kind, TypedExprKind::ColumnRef { scope_depth: 0, .. })
        });
        if has_row_variable {
            return Err(AnalyzerError::Unsupported(format!(
                "argument of {} must not contain variables",
                clause
            )));
        }
        Ok(())
    }

    fn resolve_projection_param_type(
        &mut self,
        mut expr: TypedExpr,
    ) -> Result<TypedExpr, AnalyzerError> {
        // In a SELECT target list, an otherwise-unresolved parameter behaves like
        // PostgreSQL's unknown literal in output context and defaults to text.
        if let TypedExprKind::Parameter { index } = &expr.kind {
            let was_unresolved = self.is_unresolved_param(&expr);
            self.resolve_param_type(*index, &DataType::Text)?;
            if was_unresolved {
                expr = TypedExpr::new(TypedExprKind::Parameter { index: *index }, DataType::Text);
            }
        }
        Ok(expr)
    }

    fn resolve_limit_offset_param_type(
        &mut self,
        mut expr: TypedExpr,
    ) -> Result<TypedExpr, AnalyzerError> {
        if let TypedExprKind::Parameter { index } = &expr.kind {
            let was_unresolved = self.is_unresolved_param(&expr);
            self.resolve_param_type(*index, &DataType::Int64)?;
            if was_unresolved {
                expr = TypedExpr::new(TypedExprKind::Parameter { index: *index }, DataType::Int64);
            }
        }
        Ok(expr)
    }

    /// Analyze VALUES rows and unify each column's type across all rows.
    fn analyze_values_rows(
        &mut self,
        rows: &[Vec<Expr>],
    ) -> Result<
        (
            Vec<Vec<TypedExpr>>,
            Vec<(
                String,
                DataType,
                Option<crate::sql::collation::ResolvedCollation>,
            )>,
        ),
        AnalyzerError,
    > {
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

            output_schema.push((format!("column{}", col_idx + 1), target_type, None));
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

    // -- DISTINCT --

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

    // -- CTE analysis --

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

                // Extract collation names from projection expressions (if SELECT body).
                let coll_names: Vec<Option<String>> = extract_output_collation_names(&analyzed);

                // Determine output columns (may be overridden by explicit column list)
                let columns: Vec<(String, DataType, Option<String>)> =
                    if cte.alias.columns.is_empty() {
                        analyzed
                            .output_schema
                            .iter()
                            .zip(coll_names)
                            .map(|((name, dt, _coll), cn)| (name.clone(), dt.clone(), cn))
                            .collect()
                    } else {
                        cte.alias
                            .columns
                            .iter()
                            .zip(analyzed.output_schema.iter())
                            .zip(coll_names)
                            .map(|((alias_col, (_, dt, _coll)), cn)| {
                                (alias_col.value.clone(), dt.clone(), cn)
                            })
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
