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

use crate::sql::names::split_object_name;
use crate::sql::table_functions::table_function_key;
use crate::sql::types::coercion::common_type;
use crate::types::DataType;

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
    fn ensure_boolean(&self, expr: &TypedExpr, context: &str) -> Result<(), AnalyzerError> {
        if expr.data_type != DataType::Boolean {
            return Err(AnalyzerError::TypeMismatch {
                expected: DataType::Boolean,
                found: expr.data_type.clone(),
                context: context.to_string(),
            });
        }
        Ok(())
    }

    /// Analyze a complete SQL query (top-level entry point).
    ///
    /// Handles WITH clause (CTEs), query body (SELECT / set operations),
    /// and query-level ORDER BY / LIMIT / OFFSET.
    pub fn analyze_query(&mut self, query: &Query) -> Result<AnalyzedQuery, AnalyzerError> {
        match &*query.body {
            SetExpr::Select(select) => {
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

    /// Analyze a SetExpr (used for set operations' left/right branches).
    ///
    /// Each branch manages its own scope lifecycle independently.
    fn analyze_set_expr(&mut self, set_expr: &SetExpr) -> Result<AnalyzedQuery, AnalyzerError> {
        match set_expr {
            SetExpr::Select(select) => self.analyze_select(select),
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
                self.ensure_boolean(&analyzed, "WHERE clause")?;
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
                self.ensure_boolean(&analyzed, "HAVING clause")?;
                Some(analyzed)
            }
            None => None,
        };

        // 6. SELECT list (aggregates allowed)
        let (projection, output_schema) = self.analyze_projection(&select.projection)?;

        // 7. DISTINCT
        let distinct = self.analyze_distinct(&select.distinct)?;

        // 8. ORDER BY (can reference output aliases + FROM columns)
        let analyzed_order_by = self.analyze_order_by_exprs(order_by, &projection)?;

        // 9. LIMIT
        let analyzed_limit = match limit {
            Some(l) => Some(self.analyze_expr(l)?),
            None => None,
        };

        // 10. OFFSET
        let analyzed_offset = match offset {
            Some(o) => Some(self.analyze_expr(&o.value)?),
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

    fn analyze_table_with_joins(
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
                    self.scopes
                        .current_mut()
                        .hide_using_column(right_start + uc.right_index);
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

                let (_schema_opt, obj_name) = split_object_name(name)
                    .map_err(|e| AnalyzerError::Unsupported(e.to_string()))?;
                let alias_str = alias
                    .as_ref()
                    .map(|a| a.name.value.clone())
                    .unwrap_or_else(|| obj_name.clone());

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
                    name: obj_name.to_ascii_uppercase(),
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
                            .map(|a| a.name.value.clone())
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
                    .map(|a| a.name.value.clone())
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
                    .map(|a| a.name.value.clone())
                    .unwrap_or_else(|| "subquery".to_string());

                // Add subquery output columns to current scope
                let columns: Vec<(String, DataType, bool)> = analyzed
                    .output_schema
                    .iter()
                    .map(|(name, dt)| (name.clone(), dt.clone(), true))
                    .collect();
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
                    result.alias = Some(a.name.value.clone());
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
                self.ensure_boolean(&analyzed, "JOIN ON clause")?;
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
                            left: left_col.data_type.clone(),
                            right: right_col.data_type.clone(),
                        })?;

                    resolved.push(ResolvedUsingColumn {
                        name: col_name.clone(),
                        left_index: left_col.column_index - left_start,
                        right_index: right_col.column_index - right_start,
                        data_type: unified_type,
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
                                    left: lc.data_type.clone(),
                                    right: rc.data_type.clone(),
                                }
                            })?;
                        resolved.push(ResolvedUsingColumn {
                            name: lc.column_name.clone(),
                            left_index: lc.column_index - left_start,
                            right_index: rc.column_index - right_start,
                            data_type: unified_type,
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
                        let name_lower = ident.value.to_lowercase();
                        for item in select_items {
                            if let SelectItem::ExprWithAlias { expr, alias } = item {
                                if alias.value.to_lowercase() == name_lower {
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

    // ── SELECT projection ───────────────────────────────────

    fn analyze_projection(
        &mut self,
        items: &[SelectItem],
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
                    for col in scope.columns() {
                        // Skip USING join right-side duplicate columns.
                        if col.hidden {
                            continue;
                        }
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

                SelectItem::QualifiedWildcard(name, _) => {
                    let table_name = name.to_string();
                    let scope = self.scopes.current();
                    let lower_table = table_name.to_lowercase();

                    let matching_cols: Vec<_> = scope
                        .columns()
                        .iter()
                        .filter(|c| {
                            !c.hidden
                                && c.table_alias
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
        match expr {
            Expr::Identifier(ident) => ident.value.clone(),
            Expr::CompoundIdentifier(parts) => parts
                .last()
                .map(|i| i.value.clone())
                .unwrap_or_else(|| "?column?".to_string()),
            Expr::Function(func) => crate::sql::names::function_name_upper(func).to_lowercase(),
            _ => "?column?".to_string(),
        }
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
