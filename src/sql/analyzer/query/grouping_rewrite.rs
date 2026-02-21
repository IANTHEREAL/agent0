//! GROUPING SETS / ROLLUP / CUBE rewriting.
//!
//! Rewrites queries with GROUPING SETS, ROLLUP, or CUBE into UNION ALL over
//! simple GROUP BY arms so the analyzed pipeline remains single-path.

use sqlparser::ast::{self as ast, Expr, Query, Select, SelectItem, SetExpr};

use std::collections::HashSet;

use crate::sql::names::normalize_ident;

use super::super::error::AnalyzerError;
use super::super::Analyzer;

impl<'a> Analyzer<'a> {
    pub(super) fn maybe_rewrite_grouping_sets_query(
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

    pub(super) fn column_expr_keys(expr: &Expr) -> Option<Vec<String>> {
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

    pub(super) fn expr_is_grouped_column(grouped_keys: &HashSet<String>, expr: &Expr) -> bool {
        Self::column_expr_keys(expr)
            .map(|keys| keys.into_iter().any(|k| grouped_keys.contains(&k)))
            .unwrap_or(false)
    }

    pub(super) fn default_alias_for_expr(expr: &Expr) -> Option<ast::Ident> {
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

    pub(super) fn rewrite_expr_for_group_set(
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
}
