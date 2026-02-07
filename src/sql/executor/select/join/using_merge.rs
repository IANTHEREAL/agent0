use super::super::*;

/// Tracks a column merged by USING or NATURAL JOIN.
/// Per SQL standard, unqualified references resolve to COALESCE(left.col, right.col, ...).
pub(in crate::sql::executor::select) struct UsingMergeColumn {
    pub(in crate::sql::executor::select) col_name: String,
    pub(in crate::sql::executor::select) source_aliases: Vec<String>,
}

/// Build `COALESCE(t1.col, t2.col, ...)` AST expression for a merge column.
pub(in crate::sql::executor::select) fn build_coalesce_for_merge(mc: &UsingMergeColumn) -> Expr {
    let args: Vec<FunctionArg> = mc
        .source_aliases
        .iter()
        .map(|alias| {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::CompoundIdentifier(vec![
                Ident::new(alias.clone()),
                Ident::new(mc.col_name.clone()),
            ])))
        })
        .collect();
    Expr::Function(Function {
        name: ObjectName(vec![Ident::new("COALESCE")]),
        args,
        filter: None,
        null_treatment: None,
        over: None,
        distinct: false,
        special: false,
        order_by: vec![],
    })
}

/// Replace bare-identifier references to merge columns with COALESCE expressions.
/// Qualified references (e.g. `t.col`) are left untouched.
pub(in crate::sql::executor::select) fn replace_using_merge_refs(
    expr: &Expr,
    merge_columns: &[UsingMergeColumn],
) -> Expr {
    if merge_columns.is_empty() {
        return expr.clone();
    }
    match expr {
        Expr::Identifier(ident) => {
            for mc in merge_columns {
                if mc.col_name.eq_ignore_ascii_case(&ident.value) {
                    return build_coalesce_for_merge(mc);
                }
            }
            expr.clone()
        }
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: Box::new(replace_using_merge_refs(left, merge_columns)),
            op: op.clone(),
            right: Box::new(replace_using_merge_refs(right, merge_columns)),
        },
        Expr::UnaryOp { op, expr: inner } => Expr::UnaryOp {
            op: op.clone(),
            expr: Box::new(replace_using_merge_refs(inner, merge_columns)),
        },
        Expr::Nested(inner) => {
            Expr::Nested(Box::new(replace_using_merge_refs(inner, merge_columns)))
        }
        Expr::IsNull(inner) => {
            Expr::IsNull(Box::new(replace_using_merge_refs(inner, merge_columns)))
        }
        Expr::IsNotNull(inner) => {
            Expr::IsNotNull(Box::new(replace_using_merge_refs(inner, merge_columns)))
        }
        Expr::Function(f) => {
            let new_args = f
                .args
                .iter()
                .map(|arg| match arg {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => FunctionArg::Unnamed(
                        FunctionArgExpr::Expr(replace_using_merge_refs(e, merge_columns)),
                    ),
                    other => other.clone(),
                })
                .collect();
            let new_order_by = f
                .order_by
                .iter()
                .map(|o| sqlparser::ast::OrderByExpr {
                    expr: replace_using_merge_refs(&o.expr, merge_columns),
                    asc: o.asc,
                    nulls_first: o.nulls_first,
                })
                .collect();
            Expr::Function(Function {
                args: new_args,
                order_by: new_order_by,
                ..f.clone()
            })
        }
        Expr::Cast {
            expr: inner,
            data_type,
            format,
        } => Expr::Cast {
            expr: Box::new(replace_using_merge_refs(inner, merge_columns)),
            data_type: data_type.clone(),
            format: format.clone(),
        },
        Expr::InList {
            expr: inner,
            list,
            negated,
        } => Expr::InList {
            expr: Box::new(replace_using_merge_refs(inner, merge_columns)),
            list: list
                .iter()
                .map(|e| replace_using_merge_refs(e, merge_columns))
                .collect(),
            negated: *negated,
        },
        Expr::Between {
            expr: inner,
            negated,
            low,
            high,
        } => Expr::Between {
            expr: Box::new(replace_using_merge_refs(inner, merge_columns)),
            negated: *negated,
            low: Box::new(replace_using_merge_refs(low, merge_columns)),
            high: Box::new(replace_using_merge_refs(high, merge_columns)),
        },
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => Expr::Case {
            operand: operand
                .as_ref()
                .map(|e| Box::new(replace_using_merge_refs(e, merge_columns))),
            conditions: conditions
                .iter()
                .map(|e| replace_using_merge_refs(e, merge_columns))
                .collect(),
            results: results
                .iter()
                .map(|e| replace_using_merge_refs(e, merge_columns))
                .collect(),
            else_result: else_result
                .as_ref()
                .map(|e| Box::new(replace_using_merge_refs(e, merge_columns))),
        },
        _ => expr.clone(),
    }
}

pub(in crate::sql::executor::select) fn rewrite_for_using_join(
    expr: &Expr,
    table_aliases: &[(String, TableSchema)],
    merge_columns: &[UsingMergeColumn],
) -> Result<Expr> {
    if !merge_columns.is_empty() {
        check_using_comma_ambiguity(expr, table_aliases, merge_columns)?;
    }
    let processed = replace_using_merge_refs(expr, merge_columns);
    rewrite_expr_for_multi_join(&processed, table_aliases)
}

pub(in crate::sql::executor::select) fn check_using_comma_ambiguity(
    expr: &Expr,
    table_aliases: &[(String, TableSchema)],
    merge_columns: &[UsingMergeColumn],
) -> Result<()> {
    match expr {
        Expr::Identifier(ident) => {
            let col_name = &ident.value;
            let is_merge = merge_columns
                .iter()
                .any(|mc| mc.col_name.eq_ignore_ascii_case(col_name));
            if is_merge {
                let merge_alias_set: HashSet<String> = merge_columns
                    .iter()
                    .filter(|mc| mc.col_name.eq_ignore_ascii_case(col_name))
                    .flat_map(|mc| mc.source_aliases.iter().cloned())
                    .map(|a| a.to_lowercase())
                    .collect();
                for (alias, schema) in table_aliases {
                    if merge_alias_set.contains(&alias.to_lowercase()) {
                        continue;
                    }
                    if schema
                        .columns
                        .iter()
                        .any(|c| c.name.eq_ignore_ascii_case(col_name))
                    {
                        return Err(SqlError::AmbiguousColumn(col_name.to_string()).into());
                    }
                }
            }
            Ok(())
        }
        Expr::BinaryOp { left, right, .. } => {
            check_using_comma_ambiguity(left, table_aliases, merge_columns)?;
            check_using_comma_ambiguity(right, table_aliases, merge_columns)
        }
        Expr::UnaryOp { expr: inner, .. }
        | Expr::Nested(inner)
        | Expr::IsNull(inner)
        | Expr::IsNotNull(inner) => {
            check_using_comma_ambiguity(inner, table_aliases, merge_columns)
        }
        Expr::Function(f) => {
            for arg in &f.args {
                if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = arg {
                    check_using_comma_ambiguity(e, table_aliases, merge_columns)?;
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}
