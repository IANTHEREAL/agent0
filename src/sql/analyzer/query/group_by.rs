//! GROUP BY analysis and grouping semantics validation.
//!
//! Handles `analyze_group_by`, `validate_grouping_semantics`,
//! `contains_aggregate_call`, and `find_ungrouped_column`.

use sqlparser::ast::{self as ast, Expr, SelectItem};

use std::collections::HashSet;

use crate::sql::names::normalize_ident;
use crate::types::DataType;

use super::super::error::AnalyzerError;
use super::super::types::*;
use super::super::Analyzer;

impl<'a> Analyzer<'a> {
    pub(super) fn analyze_group_by(
        &mut self,
        group_by: &ast::GroupByExpr,
        select_items: &[SelectItem],
    ) -> Result<Vec<TypedExpr>, AnalyzerError> {
        match group_by {
            ast::GroupByExpr::All => Err(AnalyzerError::Unsupported("GROUP BY ALL".to_string())),
            ast::GroupByExpr::Expressions(exprs) => exprs
                .iter()
                .map(|e| {
                    // Resolve positional references: GROUP BY 1 -> SELECT item at position 1.
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
                    // Resolve alias references: GROUP BY alias -> SELECT expr with that alias.
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
    pub(super) fn validate_grouping_semantics(
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

    pub(super) fn contains_aggregate_call(expr: &TypedExpr) -> bool {
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
            TypedExprKind::Collate { expr, .. } => Self::contains_aggregate_call(expr),
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

    pub(super) fn find_ungrouped_column(
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

            TypedExprKind::Collate { expr, .. } => {
                Self::find_ungrouped_column(expr, grouped_columns, grouped_expr_keys, in_aggregate)
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
}
