use std::collections::{HashMap, HashSet};

use sqlparser::ast::SetQuantifier;

use super::value_key::serialize_values_for_key;
use crate::types::Row;

pub fn apply_union(left_rows: Vec<Row>, right_rows: Vec<Row>, is_all: bool) -> Vec<Row> {
    let mut result = left_rows;
    if is_all {
        result.extend(right_rows);
    } else {
        let existing: HashSet<Vec<u8>> = result
            .iter()
            .map(|r| serialize_values_for_key(&r.values).unwrap_or_default())
            .collect();
        for row in right_rows {
            let key = serialize_values_for_key(&row.values).unwrap_or_default();
            if !existing.contains(&key) {
                result.push(row);
            }
        }
        let mut seen = HashSet::new();
        result.retain(|r| {
            let key = serialize_values_for_key(&r.values).unwrap_or_default();
            seen.insert(key)
        });
    }
    result
}

pub fn apply_intersect(left_rows: Vec<Row>, right_rows: Vec<Row>, is_all: bool) -> Vec<Row> {
    if is_all {
        let mut right_counts: HashMap<Vec<u8>, usize> = HashMap::new();
        for row in right_rows {
            let key = serialize_values_for_key(&row.values).unwrap_or_default();
            *right_counts.entry(key).or_insert(0) += 1;
        }

        let mut result = Vec::new();
        for row in left_rows {
            let key = serialize_values_for_key(&row.values).unwrap_or_default();
            let Some(count) = right_counts.get_mut(&key) else {
                continue;
            };
            if *count == 0 {
                continue;
            }
            *count -= 1;
            result.push(row);
        }
        return result;
    }

    let right_set: HashSet<Vec<u8>> = right_rows
        .iter()
        .map(|r| serialize_values_for_key(&r.values).unwrap_or_default())
        .collect();
    let mut result: Vec<Row> = left_rows
        .into_iter()
        .filter(|r| {
            let key = serialize_values_for_key(&r.values).unwrap_or_default();
            right_set.contains(&key)
        })
        .collect();

    let mut seen = HashSet::new();
    result.retain(|r| {
        let key = serialize_values_for_key(&r.values).unwrap_or_default();
        seen.insert(key)
    });
    result
}

pub fn apply_except(left_rows: Vec<Row>, right_rows: Vec<Row>, is_all: bool) -> Vec<Row> {
    if is_all {
        let mut right_counts: HashMap<Vec<u8>, usize> = HashMap::new();
        for row in right_rows {
            let key = serialize_values_for_key(&row.values).unwrap_or_default();
            *right_counts.entry(key).or_insert(0) += 1;
        }

        let mut result = Vec::new();
        for row in left_rows {
            let key = serialize_values_for_key(&row.values).unwrap_or_default();
            if let Some(count) = right_counts.get_mut(&key) {
                if *count > 0 {
                    *count -= 1;
                    continue;
                }
            }
            result.push(row);
        }
        return result;
    }

    let right_set: HashSet<Vec<u8>> = right_rows
        .iter()
        .map(|r| serialize_values_for_key(&r.values).unwrap_or_default())
        .collect();
    let mut result: Vec<Row> = left_rows
        .into_iter()
        .filter(|r| {
            let key = serialize_values_for_key(&r.values).unwrap_or_default();
            !right_set.contains(&key)
        })
        .collect();

    let mut seen = HashSet::new();
    result.retain(|r| {
        let key = serialize_values_for_key(&r.values).unwrap_or_default();
        seen.insert(key)
    });
    result
}

pub fn is_set_quantifier_all(quantifier: &SetQuantifier) -> bool {
    matches!(quantifier, SetQuantifier::All)
}

use std::cmp::Ordering;

use sqlparser::ast::{Expr, OrderByExpr};

use super::expr::compare_values;
use crate::types::Value;

#[allow(dead_code)]
pub fn sort_rows_by_order<F>(rows: Vec<Row>, order_by: &[OrderByExpr], eval_fn: F) -> Vec<Row>
where
    F: Fn(&Expr, &Row) -> Value,
{
    let mut indexed: Vec<(usize, Row)> = rows.into_iter().enumerate().collect();
    indexed.sort_by(|(_, a), (_, b)| {
        for order_expr in order_by {
            let val_a = eval_fn(&order_expr.expr, a);
            let val_b = eval_fn(&order_expr.expr, b);
            let cmp = compare_values(&val_a, &val_b).unwrap_or(0);
            if cmp != 0 {
                let asc = order_expr.asc.unwrap_or(true);
                return if asc {
                    if cmp > 0 {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    }
                } else {
                    if cmp > 0 {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    }
                };
            }
        }
        Ordering::Equal
    });
    indexed.into_iter().map(|(_, r)| r).collect()
}

#[allow(dead_code)]
pub fn sort_rows_with_index<F>(
    rows: Vec<Row>,
    order_by: &[OrderByExpr],
    eval_fn: F,
) -> (Vec<Row>, Vec<usize>)
where
    F: Fn(&Expr, &Row) -> Value,
{
    let mut indexed: Vec<(usize, Row)> = rows.into_iter().enumerate().collect();
    indexed.sort_by(|(_, a), (_, b)| {
        for order_expr in order_by {
            let val_a = eval_fn(&order_expr.expr, a);
            let val_b = eval_fn(&order_expr.expr, b);
            let cmp = compare_values(&val_a, &val_b).unwrap_or(0);
            if cmp != 0 {
                let asc = order_expr.asc.unwrap_or(true);
                return if asc {
                    if cmp > 0 {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    }
                } else {
                    if cmp > 0 {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    }
                };
            }
        }
        Ordering::Equal
    });
    let indices: Vec<usize> = indexed.iter().map(|(orig_idx, _)| *orig_idx).collect();
    let rows: Vec<Row> = indexed.into_iter().map(|(_, r)| r).collect();
    (rows, indices)
}

#[allow(dead_code)]
pub fn apply_offset(rows: Vec<Row>, offset: usize) -> Vec<Row> {
    rows.into_iter().skip(offset).collect()
}

#[allow(dead_code)]
pub fn apply_limit(rows: Vec<Row>, limit: usize) -> Vec<Row> {
    rows.into_iter().take(limit).collect()
}

#[allow(dead_code)]
pub fn reorder_by_indices<T: Clone>(data: &[T], indices: &[usize]) -> Vec<T> {
    indices.iter().map(|&idx| data[idx].clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Value;

    #[test]
    fn set_operation_distinct_keys_canonicalize_floats() {
        let left = vec![Row::new(vec![Value::Float64(-0.0)])];
        let right = vec![Row::new(vec![Value::Float64(0.0)])];
        assert_eq!(apply_union(left.clone(), right.clone(), false).len(), 1);
        assert_eq!(apply_intersect(left.clone(), right.clone(), false).len(), 1);
        assert_eq!(apply_except(left, right, false).len(), 0);

        let nan1 = f64::from_bits(0x7ff8_0000_0000_0001);
        let nan2 = f64::from_bits(0x7ff8_0000_0000_0002);
        assert!(nan1.is_nan() && nan2.is_nan());
        let left = vec![Row::new(vec![Value::Float64(nan1)])];
        let right = vec![Row::new(vec![Value::Float64(nan2)])];
        assert_eq!(apply_union(left.clone(), right.clone(), false).len(), 1);
        assert_eq!(apply_intersect(left.clone(), right.clone(), false).len(), 1);
        assert_eq!(apply_except(left, right, false).len(), 0);
    }
}
