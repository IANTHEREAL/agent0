use crate::sql::executor::core::Executor;
use crate::types::{Row, Value};
use sqlparser::ast::{Expr, Value as SqlValue};

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
