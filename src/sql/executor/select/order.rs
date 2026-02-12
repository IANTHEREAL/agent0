use super::*;

pub(super) fn expand_projection_exprs_for_positional_order_by(
    resolved_projection: &[SelectItem],
    schema: &TableSchema,
) -> Vec<Expr> {
    let mut exprs = Vec::new();
    for item in resolved_projection {
        match item {
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..) => {
                exprs.extend(
                    schema
                        .columns
                        .iter()
                        .map(|col| Expr::Identifier(sqlparser::ast::Ident::new(col.name.clone()))),
                );
            }
            SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                exprs.push(expr.clone());
            }
        }
    }
    exprs
}

pub(super) fn resolve_group_by_exprs(
    group_by: &[Expr],
    resolved_projection: &[SelectItem],
    schema: &TableSchema,
) -> Result<Vec<Expr>> {
    let output_exprs = expand_projection_exprs_for_positional_order_by(resolved_projection, schema);

    group_by
        .iter()
        .map(|expr| {
            // Match PostgreSQL-ish behavior: if the name resolves to an input column, prefer it.
            // Otherwise, allow referencing select-list aliases (e.g. `SELECT ... AS day GROUP BY day`).
            if let Expr::Identifier(ref ident) = expr {
                let exists_in_schema = schema
                    .columns
                    .iter()
                    .any(|c| c.name.eq_ignore_ascii_case(&ident.value));

                if !exists_in_schema {
                    for item in resolved_projection {
                        if let SelectItem::ExprWithAlias { expr, alias } = item {
                            if alias.value.eq_ignore_ascii_case(&ident.value) {
                                return Ok(expr.clone());
                            }
                        }
                    }
                }
            }

            // Positional GROUP BY (e.g. `GROUP BY 1`).
            if let Expr::Value(SqlValue::Number(n, _)) = expr {
                if let Ok(pos) = n.parse::<usize>() {
                    if pos == 0 || pos > output_exprs.len() {
                        return Err(anyhow!("GROUP BY position {} is not in select list", pos));
                    }
                    return Ok(output_exprs[pos - 1].clone());
                }
            }

            Ok(expr.clone())
        })
        .collect()
}

pub(super) fn extract_grouping_sets(exprs: &[Expr]) -> Option<Vec<Vec<Expr>>> {
    for expr in exprs {
        match expr {
            Expr::GroupingSets(sets) => {
                return Some(sets.iter().map(|s| s.clone()).collect());
            }
            Expr::Rollup(cols) => {
                let mut sets = Vec::new();
                for i in 0..=cols.len() {
                    let mut subset = Vec::new();
                    for group in cols.iter().take(cols.len() - i) {
                        subset.extend(group.iter().cloned());
                    }
                    sets.push(subset);
                }
                return Some(sets);
            }
            Expr::Cube(cols) => {
                let n = cols.len();
                let mut sets = Vec::new();
                for mask in 0..(1usize << n) {
                    let mut subset = Vec::new();
                    for (i, group) in cols.iter().enumerate() {
                        if (mask & (1usize << i)) != 0 {
                            subset.extend(group.iter().cloned());
                        }
                    }
                    sets.push(subset);
                }
                return Some(sets);
            }
            _ => {}
        }
    }
    None
}

pub(crate) fn expr_matches(pattern: &Expr, target: &Expr) -> bool {
    match (pattern, target) {
        (Expr::Identifier(a), Expr::Identifier(b)) => a.value.eq_ignore_ascii_case(&b.value),
        (Expr::CompoundIdentifier(a), Expr::CompoundIdentifier(b)) => {
            a.len() == b.len()
                && a.iter()
                    .zip(b.iter())
                    .all(|(x, y)| x.value.eq_ignore_ascii_case(&y.value))
        }
        (Expr::Identifier(a), Expr::CompoundIdentifier(b)) => b
            .last()
            .map(|i| i.value.eq_ignore_ascii_case(&a.value))
            .unwrap_or(false),
        (Expr::CompoundIdentifier(a), Expr::Identifier(b)) => a
            .last()
            .map(|i| i.value.eq_ignore_ascii_case(&b.value))
            .unwrap_or(false),
        _ => format!("{}", pattern) == format!("{}", target),
    }
}

impl Executor {
    pub(crate) fn apply_order_by_for_aggregate(
        &self,
        rows: Vec<Row>,
        order_by: &[sqlparser::ast::OrderByExpr],
        col_names: &[String],
    ) -> anyhow::Result<Vec<Row>> {
        let mut indexed: Vec<(usize, Row)> = rows.into_iter().enumerate().collect();
        crate::sql::expr::operators::sort_by_fallible(&mut indexed, |(idx_a, a), (idx_b, b)| {
            for order_expr in order_by {
                let col_idx = match &order_expr.expr {
                    Expr::Identifier(ident) => col_names
                        .iter()
                        .position(|n| n.eq_ignore_ascii_case(&ident.value)),
                    Expr::CompoundIdentifier(parts) => parts.last().and_then(|ident| {
                        col_names
                            .iter()
                            .position(|n| n.eq_ignore_ascii_case(&ident.value))
                    }),
                    Expr::Value(SqlValue::Number(n, _)) => {
                        n.parse::<usize>().ok().map(|i| i.saturating_sub(1))
                    }
                    _ => None,
                };

                let (val_a, val_b) = if let Some(idx) = col_idx {
                    (a.values.get(idx).cloned(), b.values.get(idx).cloned())
                } else {
                    (None, None)
                };

                let val_a = val_a.unwrap_or(Value::Null);
                let val_b = val_b.unwrap_or(Value::Null);

                let asc = order_expr.asc.unwrap_or(true);
                let nulls_first = order_expr.nulls_first.unwrap_or(!asc);

                match (&val_a, &val_b) {
                    (Value::Null, Value::Null) => continue,
                    (Value::Null, _) => {
                        return Ok(if nulls_first {
                            std::cmp::Ordering::Less
                        } else {
                            std::cmp::Ordering::Greater
                        })
                    }
                    (_, Value::Null) => {
                        return Ok(if nulls_first {
                            std::cmp::Ordering::Greater
                        } else {
                            std::cmp::Ordering::Less
                        })
                    }
                    _ => {}
                }

                let cmp = crate::sql::expr::compare_values(&val_a, &val_b)?;
                if cmp != 0 {
                    return Ok(if asc {
                        if cmp > 0 {
                            std::cmp::Ordering::Greater
                        } else {
                            std::cmp::Ordering::Less
                        }
                    } else if cmp > 0 {
                        std::cmp::Ordering::Less
                    } else {
                        std::cmp::Ordering::Greater
                    });
                }
            }

            // Deterministic tie-breaker to match PostgreSQL's stable-looking output:
            // compare full output rows when ORDER BY keys are equal.
            // Unorderable columns (jsonb, vector, etc.) are skipped so that
            // ORDER BY succeeds when the ORDER BY keys themselves are orderable.
            // See #666.
            let max_cols = a.values.len().max(b.values.len());
            for i in 0..max_cols {
                let va = a.values.get(i).unwrap_or(&Value::Null);
                let vb = b.values.get(i).unwrap_or(&Value::Null);
                match crate::sql::expr::compare_values(va, vb) {
                    Ok(0) => continue,
                    Ok(cmp) => {
                        return Ok(if cmp > 0 {
                            std::cmp::Ordering::Greater
                        } else {
                            std::cmp::Ordering::Less
                        });
                    }
                    Err(_) => continue, // skip unorderable columns in tie-breaker
                }
            }

            Ok(idx_a.cmp(idx_b))
        })?;
        Ok(indexed.into_iter().map(|(_, r)| r).collect())
    }
}
