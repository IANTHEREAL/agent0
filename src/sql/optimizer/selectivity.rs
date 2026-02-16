//! Selectivity estimation for cost-based optimization.
//!
//! Estimates the fraction of rows that satisfy a predicate, using table
//! statistics collected by ANALYZE. Only called when stats are available;
//! the physical planner falls back to legacy heuristics when no stats exist.

use crate::sql::analyzer::types::{BinaryOp, IsTestKind, TypedExpr, TypedExprKind, UnaryOp};
use crate::sql::expr::compare_values;
use crate::sql::optimizer::statistics::{ColumnStatistics, TableStatistics};
use crate::types::Value;
use std::cmp::Ordering;

// ── PostgreSQL default selectivities ─────────────────────────
// Used ONLY when table stats exist but specific column stats are missing.

const DEFAULT_EQ_SEL: f64 = 0.005; // pg: 1/200
const DEFAULT_INEQ_SEL: f64 = 0.3333; // pg: 1/3
const DEFAULT_NULL_FRAC: f64 = 0.0;

// ── Public API ───────────────────────────────────────────────

/// Estimate the selectivity of a predicate given table statistics.
///
/// Only called when stats are available (caller checks).
/// Returns a value in [0.0, 1.0].
pub fn estimate_selectivity(predicate: &TypedExpr, stats: &TableStatistics) -> f64 {
    let sel = estimate_selectivity_inner(predicate, stats);
    sel.clamp(0.0, 1.0)
}

/// Estimate the number of groups for a GROUP BY clause.
///
/// Uses column-level n_distinct when available for single-column GROUP BY.
/// Falls back to `child_rows / 10` for multi-column or missing stats.
pub fn estimate_group_by_rows(
    group_by: &[TypedExpr],
    stats: &TableStatistics,
    child_rows: usize,
) -> usize {
    if group_by.len() == 1 {
        if let Some(col_name) = extract_column_name(&group_by[0]) {
            if let Some(col_stats) = get_column_stats(stats, col_name) {
                let non_null_groups = n_distinct_raw(col_stats, stats.row_count).ceil();
                let null_group = if col_stats.null_fraction > 0.0 && stats.row_count > 0 {
                    1.0
                } else {
                    0.0
                };
                return (non_null_groups + null_group).min(child_rows as f64) as usize;
            }
        }
    }
    // Multi-column or no column stats — legacy fallback.
    (child_rows / 10).max(1)
}

// ── Internal dispatch ────────────────────────────────────────

fn estimate_selectivity_inner(predicate: &TypedExpr, stats: &TableStatistics) -> f64 {
    match &predicate.kind {
        // ── Equality / inequality / comparison ───────────
        TypedExprKind::BinaryOp { op, left, right } => match op {
            BinaryOp::Eq => eq_dispatch(left, right, stats),
            BinaryOp::NotEq => noteq_dispatch(left, right, stats),
            BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq => {
                range_dispatch(op, left, right, stats)
            }
            BinaryOp::And => {
                let a = estimate_selectivity(left, stats);
                let b = estimate_selectivity(right, stats);
                a * b
            }
            BinaryOp::Or => {
                let a = estimate_selectivity(left, stats);
                let b = estimate_selectivity(right, stats);
                a + b - a * b
            }
            _ => DEFAULT_INEQ_SEL,
        },

        // ── IS NULL / IS NOT NULL ────────────────────────
        TypedExprKind::IsTest {
            expr,
            test: IsTestKind::Null,
            negated,
        } => is_null_selectivity(expr, *negated, stats),

        // IS TRUE / IS FALSE / IS UNKNOWN — default
        TypedExprKind::IsTest { .. } => DEFAULT_INEQ_SEL,

        // ── BETWEEN ──────────────────────────────────────
        TypedExprKind::Between {
            expr,
            low,
            high,
            negated,
        } => between_dispatch(expr, low, high, *negated, stats),

        // ── IN list ──────────────────────────────────────
        TypedExprKind::InList {
            expr,
            list,
            negated,
        } => in_list_dispatch(expr, list, *negated, stats),

        // ── NOT ──────────────────────────────────────────
        TypedExprKind::UnaryOp {
            op: UnaryOp::Not,
            operand,
        } => not_dispatch(operand, stats),

        // ── Everything else ──────────────────────────────
        _ => DEFAULT_INEQ_SEL,
    }
}

// ── Equality ─────────────────────────────────────────────────

fn eq_dispatch(left: &TypedExpr, right: &TypedExpr, stats: &TableStatistics) -> f64 {
    if let Some((col_name, val)) = extract_column_and_value(left, right) {
        if matches!(val, Value::Null) {
            return 0.0; // D5: col = NULL → UNKNOWN → 0
        }
        if let Some(col_stats) = get_column_stats(stats, col_name) {
            return eq_selectivity(col_stats, val, stats.row_count);
        }
        return DEFAULT_EQ_SEL;
    }
    DEFAULT_EQ_SEL
}

fn noteq_dispatch(left: &TypedExpr, right: &TypedExpr, stats: &TableStatistics) -> f64 {
    if let Some((col_name, val)) = extract_column_and_value(left, right) {
        if matches!(val, Value::Null) {
            return 0.0; // D5: col != NULL → UNKNOWN → 0
        }
        let null_frac = get_null_fraction(stats, col_name);
        let eq_sel = if let Some(col_stats) = get_column_stats(stats, col_name) {
            eq_selectivity(col_stats, val, stats.row_count)
        } else {
            DEFAULT_EQ_SEL
        };
        return ((1.0 - null_frac) - eq_sel).clamp(0.0, 1.0);
    }
    (1.0 - DEFAULT_NULL_FRAC) - DEFAULT_EQ_SEL
}

/// MCV lookup → uniform distribution fallback.
fn eq_selectivity(col_stats: &ColumnStatistics, value: &Value, row_count: usize) -> f64 {
    // MCV match
    for (i, mcv) in col_stats.most_common_vals.iter().enumerate() {
        if try_compare(mcv, value) == Some(Ordering::Equal) {
            return col_stats
                .most_common_freqs
                .get(i)
                .copied()
                .unwrap_or(DEFAULT_EQ_SEL);
        }
    }
    // Uniform distribution among non-MCV values
    let eff = n_distinct_for_eq(col_stats, row_count);
    let mcv_count = col_stats.most_common_vals.len() as f64;
    if eff <= mcv_count {
        return DEFAULT_EQ_SEL;
    }
    let sum_mcv_freq: f64 = col_stats.most_common_freqs.iter().sum();
    let remaining_frac = 1.0 - col_stats.null_fraction - sum_mcv_freq;
    let remaining_distinct = eff - mcv_count;
    (remaining_frac / remaining_distinct).clamp(0.0, 1.0)
}

// ── Range (comparison) ───────────────────────────────────────

fn range_dispatch(
    op: &BinaryOp,
    left: &TypedExpr,
    right: &TypedExpr,
    stats: &TableStatistics,
) -> f64 {
    // Try col op const
    if let (Some(col_name), Some(val)) = (extract_column_name(left), extract_constant_value(right))
    {
        if matches!(val, Value::Null) {
            return 0.0; // D5
        }
        if let Some(col_stats) = get_column_stats(stats, col_name) {
            return range_selectivity_for_op(col_stats, val, op);
        }
        return DEFAULT_INEQ_SEL;
    }
    // Try const op col (flip)
    if let (Some(val), Some(col_name)) = (extract_constant_value(left), extract_column_name(right))
    {
        if matches!(val, Value::Null) {
            return 0.0; // D5
        }
        let flipped = flip_comparison(op);
        if let Some(col_stats) = get_column_stats(stats, col_name) {
            return range_selectivity_for_op(col_stats, val, &flipped);
        }
        return DEFAULT_INEQ_SEL;
    }
    DEFAULT_INEQ_SEL
}

fn range_selectivity_for_op(col_stats: &ColumnStatistics, value: &Value, op: &BinaryOp) -> f64 {
    match op {
        BinaryOp::Lt => range_selectivity(col_stats, value, true),
        BinaryOp::LtEq => {
            // sel(col <= x) = sel(col < x) + sel(col = x)
            let lt_sel = range_selectivity(col_stats, value, true);
            // We don't have row_count here but eq_sel is small relative to range
            // Approximate: for <= we use the same histogram fraction since equi-depth
            // buckets treat boundary as inclusive
            lt_sel // Approximate: histogram mid-bucket already covers <=
        }
        BinaryOp::Gt => range_selectivity(col_stats, value, false),
        BinaryOp::GtEq => {
            let gt_sel = range_selectivity(col_stats, value, false);
            gt_sel
        }
        _ => DEFAULT_INEQ_SEL,
    }
}

/// Histogram-based range selectivity.
///
/// For `is_less_than = true`: estimates sel(col < value).
/// For `is_less_than = false`: estimates sel(col > value).
fn range_selectivity(col_stats: &ColumnStatistics, value: &Value, is_less_than: bool) -> f64 {
    let bounds = &col_stats.histogram_bounds;
    if bounds.is_empty() {
        return DEFAULT_INEQ_SEL;
    }
    let num_buckets = bounds.len() - 1;
    if num_buckets == 0 {
        return DEFAULT_INEQ_SEL;
    }

    // Find position via binary search
    let fraction = match histogram_fraction(bounds, value) {
        Some(f) => f,
        None => return DEFAULT_INEQ_SEL, // incomparable types
    };

    let sel = if is_less_than {
        fraction * (1.0 - col_stats.null_fraction)
    } else {
        (1.0 - fraction) * (1.0 - col_stats.null_fraction)
    };
    sel.clamp(0.0, 1.0)
}

/// Find the fraction of the histogram that falls below `value`.
///
/// Returns `None` if bounds has fewer than 2 elements (no buckets) or if
/// values are incomparable.
fn histogram_fraction(bounds: &[Value], value: &Value) -> Option<f64> {
    if bounds.len() < 2 {
        return None;
    }
    let num_buckets = bounds.len() - 1;

    // Below first bound
    if try_compare(value, &bounds[0])? == Ordering::Less {
        return Some(0.0);
    }
    // At or above last bound
    if try_compare(value, &bounds[bounds.len() - 1])? != Ordering::Less {
        return Some(1.0);
    }

    // Linear search for the bucket containing value
    for i in 0..num_buckets {
        let cmp_low = try_compare(value, &bounds[i])?;

        if cmp_low == Ordering::Equal {
            // Exactly at bucket boundary i
            return Some(i as f64 / num_buckets as f64);
        }
        if cmp_low == Ordering::Greater {
            let cmp_high = try_compare(value, &bounds[i + 1])?;
            if cmp_high == Ordering::Less {
                // Strictly inside bucket i — mid-bucket assumption
                return Some((i as f64 + 0.5) / num_buckets as f64);
            }
            // If cmp_high == Equal, the next iteration catches it via cmp_low == Equal
        }
    }

    // Shouldn't reach here if bounds are sorted, but fallback
    Some(0.5)
}

// ── BETWEEN ──────────────────────────────────────────────────

fn between_dispatch(
    expr: &TypedExpr,
    low: &TypedExpr,
    high: &TypedExpr,
    negated: bool,
    stats: &TableStatistics,
) -> f64 {
    if let Some(col_name) = extract_column_name(expr) {
        if let (Some(low_val), Some(high_val)) =
            (extract_constant_value(low), extract_constant_value(high))
        {
            if matches!(low_val, Value::Null) || matches!(high_val, Value::Null) {
                return 0.0; // D5
            }
            if let Some(col_stats) = get_column_stats(stats, col_name) {
                // Both sides must succeed; if either returns None (incomparable
                // types or empty histogram), fall through to the default path.
                if let (Some(frac_high), Some(frac_low)) = (
                    histogram_fraction(&col_stats.histogram_bounds, high_val),
                    histogram_fraction(&col_stats.histogram_bounds, low_val),
                ) {
                    let nonnull = 1.0 - col_stats.null_fraction;
                    let between_sel = (frac_high * nonnull - frac_low * nonnull).clamp(0.0, 1.0);
                    if negated {
                        return ((1.0 - col_stats.null_fraction) - between_sel).clamp(0.0, 1.0);
                    }
                    return between_sel;
                }
                // Histogram unusable — fall through to default with real null_frac.
            }
        }
        // Column stats missing or non-constant bounds
        let null_frac = get_null_fraction(stats, col_name);
        if negated {
            return ((1.0 - null_frac) - DEFAULT_INEQ_SEL).clamp(0.0, 1.0);
        }
        return DEFAULT_INEQ_SEL;
    }
    if negated {
        ((1.0 - DEFAULT_NULL_FRAC) - DEFAULT_INEQ_SEL).clamp(0.0, 1.0)
    } else {
        DEFAULT_INEQ_SEL
    }
}

// ── IN list ──────────────────────────────────────────────────

fn in_list_dispatch(
    expr: &TypedExpr,
    list: &[TypedExpr],
    negated: bool,
    stats: &TableStatistics,
) -> f64 {
    // NOT IN with NULL literal → 0.0 (D5)
    if negated {
        for item in list {
            if let Some(val) = extract_constant_value(item) {
                if matches!(val, Value::Null) {
                    return 0.0;
                }
            }
            // Non-constant items: we can't detect runtime NULL, accepted approximation
        }
    }

    if let Some(col_name) = extract_column_name(expr) {
        let col_stats = get_column_stats(stats, col_name);
        let in_sel = in_list_selectivity(col_stats, list, stats.row_count);
        if negated {
            let null_frac = get_null_fraction(stats, col_name);
            return ((1.0 - null_frac) - in_sel).clamp(0.0, 1.0);
        }
        return in_sel.clamp(0.0, 1.0);
    }

    // No recognizable column
    let n = list.len() as f64;
    let in_sel = (n * DEFAULT_EQ_SEL).min(1.0);
    if negated {
        ((1.0 - DEFAULT_NULL_FRAC) - in_sel).clamp(0.0, 1.0)
    } else {
        in_sel
    }
}

/// Sum eq selectivities over deduplicated constant values.
fn in_list_selectivity(
    col_stats: Option<&ColumnStatistics>,
    list: &[TypedExpr],
    row_count: usize,
) -> f64 {
    let mut unique_values: Vec<&Value> = Vec::new();
    let mut non_constant_count: usize = 0;

    for item in list {
        if let Some(val) = extract_constant_value(item) {
            if matches!(val, Value::Null) {
                continue; // D5: NULL contributes 0
            }
            let is_dup = unique_values
                .iter()
                .any(|u| try_compare(u, val) == Some(Ordering::Equal));
            if !is_dup {
                unique_values.push(val);
            }
        } else {
            non_constant_count += 1;
        }
    }

    let mut sel: f64 = match col_stats {
        Some(cs) => unique_values
            .iter()
            .map(|v| eq_selectivity(cs, v, row_count))
            .sum(),
        None => unique_values.len() as f64 * DEFAULT_EQ_SEL,
    };

    sel += non_constant_count as f64 * DEFAULT_EQ_SEL;
    sel.min(1.0)
}

// ── IS NULL ──────────────────────────────────────────────────

fn is_null_selectivity(expr: &TypedExpr, negated: bool, stats: &TableStatistics) -> f64 {
    let null_frac = if let Some(col_name) = extract_column_name(expr) {
        get_null_fraction(stats, col_name)
    } else {
        DEFAULT_NULL_FRAC
    };
    if negated {
        1.0 - null_frac
    } else {
        null_frac
    }
}

// ── NOT normalization ────────────────────────────────────────

fn not_dispatch(operand: &TypedExpr, stats: &TableStatistics) -> f64 {
    match &operand.kind {
        // NOT (col = c) → col != c (null-safe)
        TypedExprKind::BinaryOp {
            op: BinaryOp::Eq,
            left,
            right,
        } => noteq_dispatch(left, right, stats),

        // NOT (col != c) → col = c
        TypedExprKind::BinaryOp {
            op: BinaryOp::NotEq,
            left,
            right,
        } => eq_dispatch(left, right, stats),

        // NOT (col IN (...)) → col NOT IN (...) (null-safe)
        TypedExprKind::InList {
            expr,
            list,
            negated: false,
        } => in_list_dispatch(expr, list, true, stats),

        // NOT (col NOT IN (...)) → col IN (...)
        TypedExprKind::InList {
            expr,
            list,
            negated: true,
        } => in_list_dispatch(expr, list, false, stats),

        // NOT (col BETWEEN a AND b) → col NOT BETWEEN a AND b (null-safe)
        TypedExprKind::Between {
            expr,
            low,
            high,
            negated: false,
        } => between_dispatch(expr, low, high, true, stats),

        // NOT (col NOT BETWEEN a AND b) → col BETWEEN a AND b
        TypedExprKind::Between {
            expr,
            low,
            high,
            negated: true,
        } => between_dispatch(expr, low, high, false, stats),

        // NOT (IS NULL) → IS NOT NULL, NOT (IS NOT NULL) → IS NULL
        TypedExprKind::IsTest {
            expr,
            test: IsTestKind::Null,
            negated,
        } => is_null_selectivity(expr, !negated, stats),

        // NOT (NOT inner) → inner (double negation elimination)
        TypedExprKind::UnaryOp {
            op: UnaryOp::Not,
            operand: inner,
        } => estimate_selectivity(inner, stats),

        // Generic fallback
        _ => 1.0 - estimate_selectivity(operand, stats),
    }
}

// ── Helper: n_distinct ───────────────────────────────────────

/// Raw effective n_distinct — can be 0.0 for all-NULL columns.
/// Used by GROUP BY estimation where 0 non-null groups is a valid answer.
fn n_distinct_raw(col: &ColumnStatistics, row_count: usize) -> f64 {
    if col.n_distinct >= 0.0 {
        col.n_distinct
    } else {
        (-col.n_distinct) * row_count as f64
    }
}

/// Division-safe effective n_distinct — always >= 1.0.
/// Used in equality selectivity formulas where n_distinct appears as a divisor.
fn n_distinct_for_eq(col: &ColumnStatistics, row_count: usize) -> f64 {
    n_distinct_raw(col, row_count).max(1.0)
}

// ── Helper: column lookup (case-insensitive, ambiguity-safe) ─

fn get_column_stats<'a>(
    stats: &'a TableStatistics,
    col_name: &str,
) -> Option<&'a ColumnStatistics> {
    // Exact match first (fast path)
    if let Some(cs) = stats.columns.get(col_name) {
        return Some(cs);
    }
    // Case-insensitive fallback — only if unambiguous
    let matches: Vec<_> = stats
        .columns
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case(col_name))
        .collect();
    if matches.len() == 1 {
        Some(matches[0].1)
    } else {
        None
    }
}

fn get_null_fraction(stats: &TableStatistics, col_name: &str) -> f64 {
    get_column_stats(stats, col_name)
        .map(|cs| cs.null_fraction)
        .unwrap_or(DEFAULT_NULL_FRAC)
}

// ── Helper: expression extraction ────────────────────────────

fn extract_column_and_value<'a>(
    left: &'a TypedExpr,
    right: &'a TypedExpr,
) -> Option<(&'a str, &'a Value)> {
    if let (Some(name), Some(val)) = (extract_column_name(left), extract_constant_value(right)) {
        return Some((name, val));
    }
    if let (Some(val), Some(name)) = (extract_constant_value(left), extract_column_name(right)) {
        return Some((name, val));
    }
    None
}

fn extract_column_name(expr: &TypedExpr) -> Option<&str> {
    match &expr.kind {
        TypedExprKind::ColumnRef { column_name, .. } => Some(column_name),
        TypedExprKind::Cast { expr: inner, .. } => extract_column_name(inner),
        _ => None,
    }
}

fn extract_constant_value(expr: &TypedExpr) -> Option<&Value> {
    match &expr.kind {
        TypedExprKind::Constant(v) => Some(v),
        TypedExprKind::Cast { expr: inner, .. } => extract_constant_value(inner),
        _ => None,
    }
}

fn flip_comparison(op: &BinaryOp) -> BinaryOp {
    match op {
        BinaryOp::Lt => BinaryOp::Gt,
        BinaryOp::LtEq => BinaryOp::GtEq,
        BinaryOp::Gt => BinaryOp::Lt,
        BinaryOp::GtEq => BinaryOp::LtEq,
        other => other.clone(),
    }
}

// ── Helper: value comparison ─────────────────────────────────

fn try_compare(left: &Value, right: &Value) -> Option<Ordering> {
    compare_values(left, right).ok().map(|c| match c {
        -1 => Ordering::Less,
        0 => Ordering::Equal,
        _ => Ordering::Greater,
    })
}

// ── Tests ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::analyzer::types::TypedExprKind;
    use crate::sql::types::CastContext;
    use crate::types::DataType;
    use std::collections::HashMap;

    // ── Test helpers ─────────────────────────────────────

    fn col_ref(name: &str) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: 0,
                column_name: name.to_string(),
            },
            data_type: DataType::Int64,
        }
    }

    fn constant(v: Value) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::Constant(v),
            data_type: DataType::Int64,
        }
    }

    fn cast_expr(inner: TypedExpr, target: DataType) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::Cast {
                expr: Box::new(inner),
                target_type: target.clone(),
                cast_context: CastContext::Implicit,
            },
            data_type: target,
        }
    }

    fn binary_op(left: TypedExpr, op: BinaryOp, right: TypedExpr) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(left),
                op,
                right: Box::new(right),
            },
            data_type: DataType::Boolean,
        }
    }

    fn not_expr(inner: TypedExpr) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::UnaryOp {
                op: UnaryOp::Not,
                operand: Box::new(inner),
            },
            data_type: DataType::Boolean,
        }
    }

    fn is_test(expr: TypedExpr, test: IsTestKind, negated: bool) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::IsTest {
                expr: Box::new(expr),
                test,
                negated,
            },
            data_type: DataType::Boolean,
        }
    }

    fn in_list(expr: TypedExpr, list: Vec<TypedExpr>, negated: bool) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::InList {
                expr: Box::new(expr),
                list,
                negated,
            },
            data_type: DataType::Boolean,
        }
    }

    fn between(expr: TypedExpr, low: TypedExpr, high: TypedExpr, negated: bool) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::Between {
                expr: Box::new(expr),
                low: Box::new(low),
                high: Box::new(high),
                negated,
            },
            data_type: DataType::Boolean,
        }
    }

    fn func_call_expr() -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::FunctionCall {
                func: crate::sql::analyzer::types::ResolvedFunction {
                    name: "test_func".to_string(),
                    kind: crate::sql::analyzer::types::FunctionKind::Builtin,
                    return_type: DataType::Int64,
                },
                args: vec![],
                order_by: vec![],
                filter: None,
            },
            data_type: DataType::Int64,
        }
    }

    fn make_stats(row_count: usize, columns: HashMap<String, ColumnStatistics>) -> TableStatistics {
        TableStatistics {
            table_id: 1,
            row_count,
            last_analyzed: 1000,
            columns,
        }
    }

    fn make_col_stats(
        null_fraction: f64,
        n_distinct: f64,
        mcvs: Vec<Value>,
        mcv_freqs: Vec<f64>,
        histogram: Vec<Value>,
    ) -> ColumnStatistics {
        ColumnStatistics {
            null_fraction,
            n_distinct,
            avg_width: 4,
            most_common_vals: mcvs,
            most_common_freqs: mcv_freqs,
            histogram_bounds: histogram,
            correlation: 0.0,
        }
    }

    // ── Test 1: Equality MCV hit ─────────────────────────

    #[test]
    fn test_eq_mcv_hit() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(
                0.0,
                10.0,
                vec![Value::Int32(1), Value::Int32(2), Value::Int32(3)],
                vec![0.3, 0.2, 0.1],
                vec![],
            ),
        );
        let stats = make_stats(1000, cols);
        let pred = binary_op(col_ref("id"), BinaryOp::Eq, constant(Value::Int32(1)));
        let sel = estimate_selectivity(&pred, &stats);
        assert!((sel - 0.3).abs() < 1e-9, "MCV hit: got {}", sel);
    }

    // ── Test 2: Equality uniform ─────────────────────────

    #[test]
    fn test_eq_uniform() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(
                0.0,
                100.0,
                vec![Value::Int32(1), Value::Int32(2), Value::Int32(3)],
                vec![0.3, 0.2, 0.1],
                vec![],
            ),
        );
        let stats = make_stats(1000, cols);
        let pred = binary_op(col_ref("id"), BinaryOp::Eq, constant(Value::Int32(999)));
        let sel = estimate_selectivity(&pred, &stats);
        // (1.0 - 0.0 - 0.6) / (100 - 3) = 0.4/97 ≈ 0.00412
        let expected = 0.4 / 97.0;
        assert!(
            (sel - expected).abs() < 1e-6,
            "uniform eq: expected {}, got {}",
            expected,
            sel
        );
    }

    // ── Test 3: Equality no column stats ─────────────────

    #[test]
    fn test_eq_no_col_stats() {
        let stats = make_stats(1000, HashMap::new());
        let pred = binary_op(col_ref("missing"), BinaryOp::Eq, constant(Value::Int32(1)));
        let sel = estimate_selectivity(&pred, &stats);
        assert!(
            (sel - DEFAULT_EQ_SEL).abs() < 1e-9,
            "no col stats: got {}",
            sel
        );
    }

    // ── Test 4: Equality negative n_distinct ─────────────

    #[test]
    fn test_eq_negative_n_distinct() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(0.0, -0.8, vec![], vec![], vec![]),
        );
        let stats = make_stats(1000, cols);
        let pred = binary_op(col_ref("id"), BinaryOp::Eq, constant(Value::Int32(42)));
        let sel = estimate_selectivity(&pred, &stats);
        // eff = 0.8 * 1000 = 800, sel = (1.0 - 0.0 - 0.0) / 800 = 1/800
        let expected = 1.0 / 800.0;
        assert!(
            (sel - expected).abs() < 1e-6,
            "neg n_distinct: expected {}, got {}",
            expected,
            sel
        );
    }

    // ── Test 5: Flipped orientation ──────────────────────

    #[test]
    fn test_flipped_orientation() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(0.0, 10.0, vec![Value::Int32(42)], vec![0.5], vec![]),
        );
        let stats = make_stats(1000, cols);
        // 42 = col (flipped)
        let pred = binary_op(constant(Value::Int32(42)), BinaryOp::Eq, col_ref("id"));
        let sel = estimate_selectivity(&pred, &stats);
        assert!((sel - 0.5).abs() < 1e-9, "flipped: got {}", sel);
    }

    // ── Test 6: Cast peeling ─────────────────────────────

    #[test]
    fn test_cast_peeling() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(0.0, 100.0, vec![Value::Int32(42)], vec![0.5], vec![]),
        );
        let stats = make_stats(1000, cols);
        let casted_col = cast_expr(col_ref("id"), DataType::Int64);
        let pred = binary_op(casted_col, BinaryOp::Eq, constant(Value::Int32(42)));
        let sel = estimate_selectivity(&pred, &stats);
        assert!((sel - 0.5).abs() < 1e-9, "cast peeling: got {}", sel);
    }

    // ── Test 7: NotEq null-safe ──────────────────────────

    #[test]
    fn test_noteq_null_safe() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(0.1, 10.0, vec![Value::Int32(1)], vec![0.3], vec![]),
        );
        let stats = make_stats(1000, cols);
        let pred = binary_op(col_ref("id"), BinaryOp::NotEq, constant(Value::Int32(1)));
        let sel = estimate_selectivity(&pred, &stats);
        // (1.0 - 0.1) - 0.3 = 0.6
        assert!((sel - 0.6).abs() < 1e-9, "noteq: got {}", sel);
    }

    // ── Test 8: IS NULL ──────────────────────────────────

    #[test]
    fn test_is_null() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(0.25, 10.0, vec![], vec![], vec![]),
        );
        let stats = make_stats(1000, cols);
        let pred = is_test(col_ref("id"), IsTestKind::Null, false);
        let sel = estimate_selectivity(&pred, &stats);
        assert!((sel - 0.25).abs() < 1e-9, "IS NULL: got {}", sel);
    }

    // ── Test 9: IS NOT NULL ──────────────────────────────

    #[test]
    fn test_is_not_null() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(0.25, 10.0, vec![], vec![], vec![]),
        );
        let stats = make_stats(1000, cols);
        let pred = is_test(col_ref("id"), IsTestKind::Null, true);
        let sel = estimate_selectivity(&pred, &stats);
        assert!((sel - 0.75).abs() < 1e-9, "IS NOT NULL: got {}", sel);
    }

    // ── Test 10: IS NULL no col stats ────────────────────

    #[test]
    fn test_is_null_no_col_stats() {
        let stats = make_stats(1000, HashMap::new());
        let pred = is_test(col_ref("missing"), IsTestKind::Null, false);
        let sel = estimate_selectivity(&pred, &stats);
        assert!(
            (sel - DEFAULT_NULL_FRAC).abs() < 1e-9,
            "IS NULL no stats: got {}",
            sel
        );
    }

    // ── Test 11: IN list ─────────────────────────────────

    #[test]
    fn test_in_list() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(
                0.0,
                100.0,
                vec![Value::Int32(1), Value::Int32(2)],
                vec![0.1, 0.05],
                vec![],
            ),
        );
        let stats = make_stats(1000, cols);
        let pred = in_list(
            col_ref("id"),
            vec![
                constant(Value::Int32(1)),
                constant(Value::Int32(2)),
                constant(Value::Int32(3)),
            ],
            false,
        );
        let sel = estimate_selectivity(&pred, &stats);
        // eq_sel(1)=0.1 + eq_sel(2)=0.05 + eq_sel(3)=uniform
        // uniform = (1.0 - 0.0 - 0.15) / (100 - 2) = 0.85/98 ≈ 0.00867
        let expected = 0.1 + 0.05 + 0.85 / 98.0;
        assert!(
            (sel - expected).abs() < 1e-4,
            "IN list: expected {}, got {}",
            expected,
            sel
        );
    }

    // ── Test 12: NOT IN ──────────────────────────────────

    #[test]
    fn test_not_in() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(
                0.1,
                100.0,
                vec![Value::Int32(1), Value::Int32(2)],
                vec![0.1, 0.05],
                vec![],
            ),
        );
        let stats = make_stats(1000, cols);
        let pred = in_list(
            col_ref("id"),
            vec![
                constant(Value::Int32(1)),
                constant(Value::Int32(2)),
                constant(Value::Int32(3)),
            ],
            true,
        );
        let sel = estimate_selectivity(&pred, &stats);
        // in_sel = 0.1 + 0.05 + uniform(3)
        // not_in = (1.0 - 0.1) - in_sel
        let uniform_3 = (1.0 - 0.1 - 0.15) / (100.0 - 2.0); // 0.75/98
        let in_sel = 0.1 + 0.05 + uniform_3;
        let expected = (1.0 - 0.1) - in_sel;
        assert!(
            (sel - expected).abs() < 1e-4,
            "NOT IN: expected {}, got {}",
            expected,
            sel
        );
    }

    // ── Test 13: IN dedup ────────────────────────────────

    #[test]
    fn test_in_dedup() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(0.0, 100.0, vec![Value::Int32(1)], vec![0.1], vec![]),
        );
        let stats = make_stats(1000, cols);
        let single = in_list(col_ref("id"), vec![constant(Value::Int32(1))], false);
        let triple = in_list(
            col_ref("id"),
            vec![
                constant(Value::Int32(1)),
                constant(Value::Int32(1)),
                constant(Value::Int32(1)),
            ],
            false,
        );
        let sel_single = estimate_selectivity(&single, &stats);
        let sel_triple = estimate_selectivity(&triple, &stats);
        assert!(
            (sel_single - sel_triple).abs() < 1e-9,
            "dedup: single={}, triple={}",
            sel_single,
            sel_triple
        );
    }

    // ── Test 14: BETWEEN ─────────────────────────────────

    #[test]
    fn test_between() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(
                0.0,
                100.0,
                vec![],
                vec![],
                vec![
                    Value::Int32(10),
                    Value::Int32(20),
                    Value::Int32(30),
                    Value::Int32(40),
                    Value::Int32(50),
                ],
            ),
        );
        let stats = make_stats(1000, cols);
        let pred = between(
            col_ref("id"),
            constant(Value::Int32(10)),
            constant(Value::Int32(30)),
            false,
        );
        let sel = estimate_selectivity(&pred, &stats);
        // sel(<=30) = fraction(30) * (1-0) = 2/4 * 1.0 = 0.5
        // sel(<10) = fraction(10) * (1-0) = 0/4 * 1.0 = 0.0
        // between = 0.5 - 0.0 = 0.5
        assert!((sel - 0.5).abs() < 1e-6, "BETWEEN: got {}", sel);
    }

    // ── Test 15: NOT BETWEEN ─────────────────────────────

    #[test]
    fn test_not_between() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(
                0.1,
                100.0,
                vec![],
                vec![],
                vec![
                    Value::Int32(10),
                    Value::Int32(20),
                    Value::Int32(30),
                    Value::Int32(40),
                    Value::Int32(50),
                ],
            ),
        );
        let stats = make_stats(1000, cols);
        let pred = between(
            col_ref("id"),
            constant(Value::Int32(10)),
            constant(Value::Int32(30)),
            true,
        );
        let sel = estimate_selectivity(&pred, &stats);
        // between_sel = sel(<=30) - sel(<10) = 0.5*(1-0.1) - 0.0*(1-0.1) = 0.45 - 0.0 = 0.45
        // not_between = (1.0 - 0.1) - 0.45 = 0.45
        let fraction_high = 2.0 / 4.0;
        let fraction_low = 0.0 / 4.0;
        let between_sel = fraction_high * (1.0 - 0.1) - fraction_low * (1.0 - 0.1);
        let expected = (1.0 - 0.1) - between_sel;
        assert!(
            (sel - expected).abs() < 1e-6,
            "NOT BETWEEN: expected {}, got {}",
            expected,
            sel
        );
    }

    // ── Test 16: Range histogram ─────────────────────────

    #[test]
    fn test_range_histogram() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(
                0.1,
                100.0,
                vec![],
                vec![],
                vec![
                    Value::Int32(10),
                    Value::Int32(20),
                    Value::Int32(30),
                    Value::Int32(40),
                    Value::Int32(50),
                ],
            ),
        );
        let stats = make_stats(1000, cols);
        let pred = binary_op(col_ref("id"), BinaryOp::Lt, constant(Value::Int32(25)));
        let sel = estimate_selectivity(&pred, &stats);
        // 25 is in bucket [20, 30), i=1, fraction = (1 + 0.5)/4 = 0.375
        // sel = 0.375 * (1 - 0.1) = 0.3375
        let expected = (1.0 + 0.5) / 4.0 * (1.0 - 0.1);
        assert!(
            (sel - expected).abs() < 1e-6,
            "range histogram: expected {}, got {}",
            expected,
            sel
        );
    }

    // ── Test 17: Range no histogram ──────────────────────

    #[test]
    fn test_range_no_histogram() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(0.0, 100.0, vec![], vec![], vec![]),
        );
        let stats = make_stats(1000, cols);
        let pred = binary_op(col_ref("id"), BinaryOp::Lt, constant(Value::Int32(25)));
        let sel = estimate_selectivity(&pred, &stats);
        assert!(
            (sel - DEFAULT_INEQ_SEL).abs() < 1e-6,
            "no histogram: got {}",
            sel
        );
    }

    // ── Test 18: Range single-bucket ─────────────────────

    #[test]
    fn test_range_single_bucket() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(
                0.0,
                100.0,
                vec![],
                vec![],
                vec![Value::Int32(10), Value::Int32(50)],
            ),
        );
        let stats = make_stats(1000, cols);
        let pred = binary_op(col_ref("id"), BinaryOp::Lt, constant(Value::Int32(30)));
        let sel = estimate_selectivity(&pred, &stats);
        // 30 is in bucket [10, 50), i=0, fraction = (0 + 0.5)/1 = 0.5
        // sel = 0.5 * 1.0 = 0.5
        assert!((sel - 0.5).abs() < 1e-6, "single bucket: got {}", sel);
    }

    // ── Test 19: Range below all bounds ──────────────────

    #[test]
    fn test_range_below_all_bounds() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(
                0.0,
                100.0,
                vec![],
                vec![],
                vec![Value::Int32(10), Value::Int32(50)],
            ),
        );
        let stats = make_stats(1000, cols);
        let pred = binary_op(col_ref("id"), BinaryOp::Lt, constant(Value::Int32(5)));
        let sel = estimate_selectivity(&pred, &stats);
        assert!(sel.abs() < 1e-9, "below all: got {}", sel);
    }

    // ── Test 20: Range above all bounds ──────────────────

    #[test]
    fn test_range_above_all_bounds() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(
                0.1,
                100.0,
                vec![],
                vec![],
                vec![Value::Int32(10), Value::Int32(50)],
            ),
        );
        let stats = make_stats(1000, cols);
        let pred = binary_op(col_ref("id"), BinaryOp::Lt, constant(Value::Int32(100)));
        let sel = estimate_selectivity(&pred, &stats);
        // fraction = 1.0, sel = 1.0 * (1 - 0.1) = 0.9
        assert!((sel - 0.9).abs() < 1e-6, "above all: got {}", sel);
    }

    // ── Test 21: AND ─────────────────────────────────────

    #[test]
    fn test_and() {
        let stats = make_stats(1000, HashMap::new());
        let a = binary_op(col_ref("a"), BinaryOp::Eq, constant(Value::Int32(1)));
        let b = binary_op(col_ref("b"), BinaryOp::Eq, constant(Value::Int32(2)));
        let pred = binary_op(a, BinaryOp::And, b);
        let sel = estimate_selectivity(&pred, &stats);
        let expected = DEFAULT_EQ_SEL * DEFAULT_EQ_SEL;
        assert!((sel - expected).abs() < 1e-9, "AND: got {}", sel);
    }

    // ── Test 22: OR ──────────────────────────────────────

    #[test]
    fn test_or() {
        let stats = make_stats(1000, HashMap::new());
        let a = binary_op(col_ref("a"), BinaryOp::Eq, constant(Value::Int32(1)));
        let b = binary_op(col_ref("b"), BinaryOp::Eq, constant(Value::Int32(2)));
        let pred = binary_op(a, BinaryOp::Or, b);
        let sel = estimate_selectivity(&pred, &stats);
        let expected = DEFAULT_EQ_SEL + DEFAULT_EQ_SEL - DEFAULT_EQ_SEL * DEFAULT_EQ_SEL;
        assert!((sel - expected).abs() < 1e-9, "OR: got {}", sel);
    }

    // ── Test 23: NOT generic ─────────────────────────────

    #[test]
    fn test_not_generic() {
        let stats = make_stats(1000, HashMap::new());
        let a = binary_op(col_ref("a"), BinaryOp::Eq, constant(Value::Int32(1)));
        let b = binary_op(col_ref("b"), BinaryOp::Eq, constant(Value::Int32(2)));
        let compound = binary_op(a, BinaryOp::And, b);
        let pred = not_expr(compound);
        let sel = estimate_selectivity(&pred, &stats);
        let inner = DEFAULT_EQ_SEL * DEFAULT_EQ_SEL;
        let expected = 1.0 - inner;
        assert!((sel - expected).abs() < 1e-9, "NOT generic: got {}", sel);
    }

    // ── Test 24: All-NULL column ─────────────────────────

    #[test]
    fn test_all_null_column() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(1.0, 0.0, vec![], vec![], vec![]),
        );
        let stats = make_stats(1000, cols);

        // eq returns 0.0 (all values are null)
        let pred = binary_op(col_ref("id"), BinaryOp::Eq, constant(Value::Int32(1)));
        let sel = estimate_selectivity(&pred, &stats);
        // remaining_frac = 1.0 - 1.0 - 0.0 = 0.0, so clamp to 0
        assert!(sel.abs() < 1e-9, "all-null eq: got {}", sel);

        // IS NULL returns 1.0
        let pred_null = is_test(col_ref("id"), IsTestKind::Null, false);
        let sel_null = estimate_selectivity(&pred_null, &stats);
        assert!(
            (sel_null - 1.0).abs() < 1e-9,
            "all-null IS NULL: got {}",
            sel_null
        );
    }

    // ── Test 25: compare_values error → default ──────────

    #[test]
    fn test_incomparable_types() {
        let mut cols = HashMap::new();
        cols.insert(
            "data".to_string(),
            make_col_stats(
                0.0,
                100.0,
                vec![],
                vec![],
                vec![
                    Value::Json("{}".to_string()),
                    Value::Json("{\"a\":1}".to_string()),
                ],
            ),
        );
        let stats = make_stats(1000, cols);
        let pred = binary_op(
            col_ref("data"),
            BinaryOp::Lt,
            constant(Value::Json("{\"b\":2}".to_string())),
        );
        let sel = estimate_selectivity(&pred, &stats);
        assert!(
            (sel - DEFAULT_INEQ_SEL).abs() < 1e-6,
            "incomparable: got {}",
            sel
        );
    }

    // ── Test 26: Selectivity bounds ──────────────────────

    #[test]
    fn test_selectivity_bounds() {
        let stats = make_stats(1000, HashMap::new());
        // All predicate types should be in [0.0, 1.0]
        let preds = vec![
            binary_op(col_ref("a"), BinaryOp::Eq, constant(Value::Int32(1))),
            binary_op(col_ref("a"), BinaryOp::NotEq, constant(Value::Int32(1))),
            binary_op(col_ref("a"), BinaryOp::Lt, constant(Value::Int32(1))),
            is_test(col_ref("a"), IsTestKind::Null, false),
            is_test(col_ref("a"), IsTestKind::Null, true),
            in_list(col_ref("a"), vec![constant(Value::Int32(1))], false),
            between(
                col_ref("a"),
                constant(Value::Int32(1)),
                constant(Value::Int32(10)),
                false,
            ),
        ];
        for pred in &preds {
            let sel = estimate_selectivity(pred, &stats);
            assert!(
                (0.0..=1.0).contains(&sel),
                "out of bounds: {} for {:?}",
                sel,
                pred.kind
            );
        }
    }

    // ── Test 27: Unrecognized predicate ──────────────────

    #[test]
    fn test_unrecognized_predicate() {
        let stats = make_stats(1000, HashMap::new());
        let pred = func_call_expr();
        let sel = estimate_selectivity(&pred, &stats);
        assert!(
            (sel - DEFAULT_INEQ_SEL).abs() < 1e-9,
            "unrecognized: got {}",
            sel
        );
    }

    // ── Test 28: NULL constant eq ────────────────────────

    #[test]
    fn test_null_constant_eq() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(0.1, 100.0, vec![], vec![], vec![]),
        );
        let stats = make_stats(1000, cols);
        let pred = binary_op(col_ref("id"), BinaryOp::Eq, constant(Value::Null));
        let sel = estimate_selectivity(&pred, &stats);
        assert!(sel.abs() < 1e-9, "NULL eq: got {}", sel);
    }

    // ── Test 29: NULL constant range ─────────────────────

    #[test]
    fn test_null_constant_range() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(
                0.0,
                100.0,
                vec![],
                vec![],
                vec![Value::Int32(1), Value::Int32(100)],
            ),
        );
        let stats = make_stats(1000, cols);
        let pred = binary_op(col_ref("id"), BinaryOp::Lt, constant(Value::Null));
        let sel = estimate_selectivity(&pred, &stats);
        assert!(sel.abs() < 1e-9, "NULL range: got {}", sel);
    }

    // ── Test 30: IN with NULL element ────────────────────

    #[test]
    fn test_in_with_null_element() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(
                0.0,
                100.0,
                vec![Value::Int32(1), Value::Int32(2)],
                vec![0.1, 0.05],
                vec![],
            ),
        );
        let stats = make_stats(1000, cols);
        let pred = in_list(
            col_ref("id"),
            vec![
                constant(Value::Int32(1)),
                constant(Value::Int32(2)),
                constant(Value::Null),
            ],
            false,
        );
        let sel = estimate_selectivity(&pred, &stats);
        // NULL skipped, so just eq_sel(1) + eq_sel(2) = 0.1 + 0.05
        let expected = 0.15;
        assert!(
            (sel - expected).abs() < 1e-6,
            "IN NULL element: expected {}, got {}",
            expected,
            sel
        );
    }

    // ── Test 31: NOT IN with NULL element ────────────────

    #[test]
    fn test_not_in_with_null() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(0.0, 100.0, vec![], vec![], vec![]),
        );
        let stats = make_stats(1000, cols);
        let pred = in_list(
            col_ref("id"),
            vec![constant(Value::Int32(1)), constant(Value::Null)],
            true,
        );
        let sel = estimate_selectivity(&pred, &stats);
        assert!(sel.abs() < 1e-9, "NOT IN with NULL: got {}", sel);
    }

    // ── Test 32: BETWEEN range-difference ────────────────

    #[test]
    fn test_between_range_difference() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(
                0.0,
                100.0,
                vec![],
                vec![],
                vec![
                    Value::Int32(10),
                    Value::Int32(20),
                    Value::Int32(30),
                    Value::Int32(40),
                    Value::Int32(50),
                ],
            ),
        );
        let stats = make_stats(1000, cols);
        let pred = between(
            col_ref("id"),
            constant(Value::Int32(15)),
            constant(Value::Int32(35)),
            false,
        );
        let sel = estimate_selectivity(&pred, &stats);
        // sel(<=35): 35 in bucket [30,40), i=2, fraction = (2+0.5)/4 = 0.625
        // sel(<15): 15 in bucket [10,20), i=0, fraction = (0+0.5)/4 = 0.125
        // between = 0.625 - 0.125 = 0.5
        let expected = 0.625 - 0.125;
        assert!(
            (sel - expected).abs() < 1e-6,
            "BETWEEN range diff: expected {}, got {}",
            expected,
            sel
        );
    }

    // ── Test 33: Case-insensitive column lookup ──────────

    #[test]
    fn test_case_insensitive_lookup() {
        let mut cols = HashMap::new();
        cols.insert(
            "ID".to_string(),
            make_col_stats(0.0, 100.0, vec![Value::Int32(1)], vec![0.5], vec![]),
        );
        let stats = make_stats(1000, cols);
        let pred = binary_op(col_ref("id"), BinaryOp::Eq, constant(Value::Int32(1)));
        let sel = estimate_selectivity(&pred, &stats);
        assert!((sel - 0.5).abs() < 1e-9, "case insensitive: got {}", sel);
    }

    // ── Test 34a: Case-insensitive lookup — exact vs ambiguous ─

    #[test]
    fn test_case_insensitive_exact_vs_ambiguous() {
        // (a) Exact match: stats has "A", predicate references "A"
        let mut cols_a = HashMap::new();
        cols_a.insert(
            "A".to_string(),
            make_col_stats(0.0, 50.0, vec![Value::Int32(1)], vec![0.4], vec![]),
        );
        let stats_a = make_stats(1000, cols_a);
        let pred_a = binary_op(
            TypedExpr {
                kind: TypedExprKind::ColumnRef {
                    scope_depth: 0,
                    column_index: 0,
                    column_name: "A".to_string(),
                },
                data_type: DataType::Int64,
            },
            BinaryOp::Eq,
            constant(Value::Int32(1)),
        );
        let sel_a = estimate_selectivity(&pred_a, &stats_a);
        assert!((sel_a - 0.4).abs() < 1e-9, "exact match A: got {}", sel_a);

        // (b) No match: stats has "A" and "a", predicate references "B"
        let mut cols_b = HashMap::new();
        cols_b.insert(
            "A".to_string(),
            make_col_stats(0.0, 50.0, vec![], vec![], vec![]),
        );
        cols_b.insert(
            "a".to_string(),
            make_col_stats(0.0, 50.0, vec![], vec![], vec![]),
        );
        let stats_b = make_stats(1000, cols_b);
        let pred_b = binary_op(
            TypedExpr {
                kind: TypedExprKind::ColumnRef {
                    scope_depth: 0,
                    column_index: 0,
                    column_name: "B".to_string(),
                },
                data_type: DataType::Int64,
            },
            BinaryOp::Eq,
            constant(Value::Int32(1)),
        );
        let sel_b = estimate_selectivity(&pred_b, &stats_b);
        assert!(
            (sel_b - DEFAULT_EQ_SEL).abs() < 1e-9,
            "no match B: got {}",
            sel_b
        );

        // (c) Ambiguous: stats has "Id" and "ID", predicate references "id"
        let mut cols_c = HashMap::new();
        cols_c.insert(
            "Id".to_string(),
            make_col_stats(0.0, 50.0, vec![Value::Int32(1)], vec![0.9], vec![]),
        );
        cols_c.insert(
            "ID".to_string(),
            make_col_stats(0.0, 50.0, vec![Value::Int32(1)], vec![0.1], vec![]),
        );
        let stats_c = make_stats(1000, cols_c);
        let pred_c = binary_op(col_ref("id"), BinaryOp::Eq, constant(Value::Int32(1)));
        let sel_c = estimate_selectivity(&pred_c, &stats_c);
        assert!(
            (sel_c - DEFAULT_EQ_SEL).abs() < 1e-9,
            "ambiguous: got {}",
            sel_c
        );
    }

    // ── Test 34b: IN list non-constant items ─────────────

    #[test]
    fn test_in_list_non_constant() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(0.0, 100.0, vec![Value::Int32(42)], vec![0.1], vec![]),
        );
        let stats = make_stats(1000, cols);
        let pred = in_list(
            col_ref("id"),
            vec![func_call_expr(), constant(Value::Int32(42))],
            false,
        );
        let sel = estimate_selectivity(&pred, &stats);
        // DEFAULT_EQ_SEL (func) + eq_sel(42) = 0.005 + 0.1 = 0.105
        let expected = DEFAULT_EQ_SEL + 0.1;
        assert!(
            (sel - expected).abs() < 1e-6,
            "IN non-const: expected {}, got {}",
            expected,
            sel
        );
    }

    // ── Test 34c: IN list all non-constant ───────────────

    #[test]
    fn test_in_list_all_non_constant() {
        let stats = make_stats(1000, HashMap::new());
        let pred = in_list(
            col_ref("id"),
            vec![func_call_expr(), func_call_expr()],
            false,
        );
        let sel = estimate_selectivity(&pred, &stats);
        let expected = 2.0 * DEFAULT_EQ_SEL;
        assert!(
            (sel - expected).abs() < 1e-9,
            "IN all non-const: expected {}, got {}",
            expected,
            sel
        );
    }

    // ── Test 34d: NotEq null-safe (null_frac=0) ─────────

    #[test]
    fn test_noteq_null_frac_zero() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(0.0, 100.0, vec![Value::Int32(1)], vec![0.1], vec![]),
        );
        let stats = make_stats(1000, cols);
        let pred = binary_op(col_ref("id"), BinaryOp::NotEq, constant(Value::Int32(1)));
        let sel = estimate_selectivity(&pred, &stats);
        let expected = (1.0 - 0.0) - 0.1; // 0.9
        assert!(
            (sel - expected).abs() < 1e-9,
            "noteq nf=0: expected {}, got {}",
            expected,
            sel
        );
    }

    // ── Test 34e: NOT IN null-safe ───────────────────────

    #[test]
    fn test_not_in_null_safe() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(
                0.2,
                100.0,
                vec![Value::Int32(1), Value::Int32(2)],
                vec![0.1, 0.05],
                vec![],
            ),
        );
        let stats = make_stats(1000, cols);
        let pred = in_list(
            col_ref("id"),
            vec![constant(Value::Int32(1)), constant(Value::Int32(2))],
            true,
        );
        let sel = estimate_selectivity(&pred, &stats);
        let in_sel = 0.1 + 0.05;
        let expected = (1.0 - 0.2) - in_sel;
        assert!(
            (sel - expected).abs() < 1e-6,
            "NOT IN null-safe: expected {}, got {}",
            expected,
            sel
        );
    }

    // ── Test 34f: NOT BETWEEN null-safe ──────────────────

    #[test]
    fn test_not_between_null_safe() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(
                0.1,
                100.0,
                vec![],
                vec![],
                vec![
                    Value::Int32(10),
                    Value::Int32(20),
                    Value::Int32(30),
                    Value::Int32(40),
                    Value::Int32(50),
                ],
            ),
        );
        let stats = make_stats(1000, cols);
        let pred = between(
            col_ref("id"),
            constant(Value::Int32(10)),
            constant(Value::Int32(30)),
            true,
        );
        let sel = estimate_selectivity(&pred, &stats);
        let fraction_high = 2.0 / 4.0; // 0.5
        let fraction_low = 0.0 / 4.0; // 0.0
        let between_sel = fraction_high * (1.0 - 0.1) - fraction_low * (1.0 - 0.1);
        let expected = (1.0 - 0.1) - between_sel;
        assert!(
            (sel - expected).abs() < 1e-6,
            "NOT BETWEEN null-safe: expected {}, got {}",
            expected,
            sel
        );
    }

    // ── Test 34g: NotEq NULL constant ────────────────────

    #[test]
    fn test_noteq_null_constant() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(0.1, 100.0, vec![], vec![], vec![]),
        );
        let stats = make_stats(1000, cols);
        let pred = binary_op(col_ref("id"), BinaryOp::NotEq, constant(Value::Null));
        let sel = estimate_selectivity(&pred, &stats);
        assert!(sel.abs() < 1e-9, "noteq NULL: got {}", sel);
    }

    // ── Test 34h: NOT normalization — NOT (col = 1) ──────

    #[test]
    fn test_not_eq_normalization() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(0.2, 10.0, vec![Value::Int32(1)], vec![0.3], vec![]),
        );
        let stats = make_stats(1000, cols);
        let inner = binary_op(col_ref("id"), BinaryOp::Eq, constant(Value::Int32(1)));
        let pred = not_expr(inner);
        let sel = estimate_selectivity(&pred, &stats);
        // Should use null-safe: (1.0 - 0.2) - 0.3 = 0.5
        let expected = (1.0 - 0.2) - 0.3;
        assert!(
            (sel - expected).abs() < 1e-9,
            "NOT (col=1): expected {}, got {}",
            expected,
            sel
        );
    }

    // ── Test 34i: NOT normalization — NOT (col IN (1,2)) ─

    #[test]
    fn test_not_in_normalization() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(
                0.1,
                100.0,
                vec![Value::Int32(1), Value::Int32(2)],
                vec![0.1, 0.05],
                vec![],
            ),
        );
        let stats = make_stats(1000, cols);
        let inner = in_list(
            col_ref("id"),
            vec![constant(Value::Int32(1)), constant(Value::Int32(2))],
            false,
        );
        let pred = not_expr(inner);
        let sel = estimate_selectivity(&pred, &stats);
        let in_sel = 0.1 + 0.05;
        let expected = (1.0 - 0.1) - in_sel;
        assert!(
            (sel - expected).abs() < 1e-6,
            "NOT (IN): expected {}, got {}",
            expected,
            sel
        );
    }

    // ── Test 34j: NOT normalization — NOT (col BETWEEN 10 AND 30) ─

    #[test]
    fn test_not_between_normalization() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(
                0.1,
                100.0,
                vec![],
                vec![],
                vec![
                    Value::Int32(10),
                    Value::Int32(20),
                    Value::Int32(30),
                    Value::Int32(40),
                    Value::Int32(50),
                ],
            ),
        );
        let stats = make_stats(1000, cols);
        let inner = between(
            col_ref("id"),
            constant(Value::Int32(10)),
            constant(Value::Int32(30)),
            false,
        );
        let pred = not_expr(inner);
        let sel = estimate_selectivity(&pred, &stats);
        let between_sel = (2.0 / 4.0 * 0.9) - (0.0 / 4.0 * 0.9);
        let expected = 0.9 - between_sel;
        assert!(
            (sel - expected).abs() < 1e-6,
            "NOT (BETWEEN): expected {}, got {}",
            expected,
            sel
        );
    }

    // ── Test 34k: NOT normalization — NOT (col IS NULL) ──

    #[test]
    fn test_not_is_null_normalization() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(0.25, 10.0, vec![], vec![], vec![]),
        );
        let stats = make_stats(1000, cols);
        let inner = is_test(col_ref("id"), IsTestKind::Null, false);
        let pred = not_expr(inner);
        let sel = estimate_selectivity(&pred, &stats);
        assert!((sel - 0.75).abs() < 1e-9, "NOT (IS NULL): got {}", sel);
    }

    // ── Test 34l: NOT generic fallback ───────────────────

    #[test]
    fn test_not_generic_fallback() {
        let stats = make_stats(1000, HashMap::new());
        let a = binary_op(col_ref("a"), BinaryOp::Eq, constant(Value::Int32(1)));
        let b = binary_op(col_ref("b"), BinaryOp::Eq, constant(Value::Int32(2)));
        let compound = binary_op(a, BinaryOp::And, b);
        let pred = not_expr(compound);
        let sel = estimate_selectivity(&pred, &stats);
        let inner = DEFAULT_EQ_SEL * DEFAULT_EQ_SEL;
        let expected = 1.0 - inner;
        assert!(
            (sel - expected).abs() < 1e-9,
            "NOT generic: expected {}, got {}",
            expected,
            sel
        );
    }

    // ── Test 34m: NOT (col != 1) → col = 1 ──────────────

    #[test]
    fn test_not_noteq_normalization() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(0.2, 10.0, vec![Value::Int32(1)], vec![0.3], vec![]),
        );
        let stats = make_stats(1000, cols);
        let inner = binary_op(col_ref("id"), BinaryOp::NotEq, constant(Value::Int32(1)));
        let pred = not_expr(inner);
        let sel = estimate_selectivity(&pred, &stats);
        assert!(
            (sel - 0.3).abs() < 1e-9,
            "NOT (!=): expected 0.3, got {}",
            sel
        );
    }

    // ── Test 34n: NOT (col NOT IN (1,2)) → col IN (1,2) ─

    #[test]
    fn test_not_not_in_normalization() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(
                0.1,
                100.0,
                vec![Value::Int32(1), Value::Int32(2)],
                vec![0.1, 0.05],
                vec![],
            ),
        );
        let stats = make_stats(1000, cols);
        let inner = in_list(
            col_ref("id"),
            vec![constant(Value::Int32(1)), constant(Value::Int32(2))],
            true,
        );
        let pred = not_expr(inner);
        let sel = estimate_selectivity(&pred, &stats);
        let expected = 0.1 + 0.05;
        assert!(
            (sel - expected).abs() < 1e-6,
            "NOT (NOT IN): expected {}, got {}",
            expected,
            sel
        );
    }

    // ── Test 34o: NOT (col NOT BETWEEN 10 AND 30) → BETWEEN ─

    #[test]
    fn test_not_not_between_normalization() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(
                0.1,
                100.0,
                vec![],
                vec![],
                vec![
                    Value::Int32(10),
                    Value::Int32(20),
                    Value::Int32(30),
                    Value::Int32(40),
                    Value::Int32(50),
                ],
            ),
        );
        let stats = make_stats(1000, cols);
        let inner = between(
            col_ref("id"),
            constant(Value::Int32(10)),
            constant(Value::Int32(30)),
            true,
        );
        let pred = not_expr(inner);
        let sel = estimate_selectivity(&pred, &stats);
        // Should be same as BETWEEN (non-negated)
        let between_pred = between(
            col_ref("id"),
            constant(Value::Int32(10)),
            constant(Value::Int32(30)),
            false,
        );
        let between_sel = estimate_selectivity(&between_pred, &stats);
        assert!(
            (sel - between_sel).abs() < 1e-9,
            "NOT (NOT BETWEEN): got {}, between={}",
            sel,
            between_sel
        );
    }

    // ── Test 34p: NOT (col IS NOT NULL) → IS NULL ────────

    #[test]
    fn test_not_is_not_null_normalization() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(0.25, 10.0, vec![], vec![], vec![]),
        );
        let stats = make_stats(1000, cols);
        let inner = is_test(col_ref("id"), IsTestKind::Null, true);
        let pred = not_expr(inner);
        let sel = estimate_selectivity(&pred, &stats);
        assert!((sel - 0.25).abs() < 1e-9, "NOT (IS NOT NULL): got {}", sel);
    }

    // ── Test 34q: NOT IN with non-literal nullable expr ──

    #[test]
    fn test_not_in_non_literal_nullable() {
        // col NOT IN (other_col, 1)
        // Since other_col is non-constant, we can't detect if it evaluates to NULL
        // at runtime. We treat it as DEFAULT_EQ_SEL. Accepted approximation.
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(0.1, 100.0, vec![Value::Int32(1)], vec![0.1], vec![]),
        );
        let stats = make_stats(1000, cols);

        // other_col is a non-constant expression (ColumnRef "other")
        let other_col = col_ref("other");
        let pred = in_list(
            col_ref("id"),
            vec![other_col, constant(Value::Int32(1))],
            true,
        );
        let sel = estimate_selectivity(&pred, &stats);
        let in_sel = DEFAULT_EQ_SEL + 0.1; // non-constant + eq_sel(1)
        let expected = ((1.0 - 0.1) - in_sel).clamp(0.0, 1.0);
        assert!(
            (sel - expected).abs() < 1e-6,
            "NOT IN non-literal: expected {}, got {}",
            expected,
            sel
        );
    }

    // ── GROUP BY tests ───────────────────────────────────

    #[test]
    fn test_group_by_basic() {
        let mut cols = HashMap::new();
        cols.insert(
            "status".to_string(),
            make_col_stats(0.0, 50.0, vec![], vec![], vec![]),
        );
        let stats = make_stats(10000, cols);
        let groups = vec![col_ref("status")];
        let result = estimate_group_by_rows(&groups, &stats, 10000);
        assert_eq!(result, 50, "GROUP BY basic: got {}", result);
    }

    #[test]
    fn test_group_by_null_group() {
        let mut cols = HashMap::new();
        cols.insert(
            "status".to_string(),
            make_col_stats(0.1, 50.0, vec![], vec![], vec![]),
        );
        let stats = make_stats(10000, cols);
        let groups = vec![col_ref("status")];
        let result = estimate_group_by_rows(&groups, &stats, 10000);
        assert_eq!(result, 51, "GROUP BY null group: got {}", result);
    }

    #[test]
    fn test_group_by_all_null() {
        let mut cols = HashMap::new();
        cols.insert(
            "status".to_string(),
            make_col_stats(1.0, 0.0, vec![], vec![], vec![]),
        );
        let stats = make_stats(10000, cols);
        let groups = vec![col_ref("status")];
        let result = estimate_group_by_rows(&groups, &stats, 10000);
        // n_distinct_raw = 0, non_null_groups = 0, null_group = 1, total = 1
        assert_eq!(result, 1, "GROUP BY all-null: got {}", result);
    }

    #[test]
    fn test_group_by_no_stats() {
        let stats = make_stats(1000, HashMap::new());
        let groups = vec![col_ref("missing")];
        let result = estimate_group_by_rows(&groups, &stats, 1000);
        assert_eq!(result, 100, "GROUP BY no stats: got {}", result);
    }

    #[test]
    fn test_group_by_multi_column() {
        let mut cols = HashMap::new();
        cols.insert(
            "a".to_string(),
            make_col_stats(0.0, 10.0, vec![], vec![], vec![]),
        );
        cols.insert(
            "b".to_string(),
            make_col_stats(0.0, 20.0, vec![], vec![], vec![]),
        );
        let stats = make_stats(1000, cols);
        let groups = vec![col_ref("a"), col_ref("b")];
        let result = estimate_group_by_rows(&groups, &stats, 1000);
        // Multi-column → legacy fallback: 1000/10 = 100
        assert_eq!(result, 100, "GROUP BY multi-col: got {}", result);
    }

    // ── Regression: P0 empty histogram in BETWEEN ────────

    #[test]
    fn test_between_empty_histogram_no_panic() {
        // ANALYZE can produce empty histograms (all values in MCV set,
        // or unorderable types). between_dispatch must not panic.
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(0.1, 100.0, vec![], vec![], vec![]), // empty histogram
        );
        let stats = make_stats(1000, cols);

        // BETWEEN — should fall back to DEFAULT_INEQ_SEL, not panic
        let pred = between(
            col_ref("id"),
            constant(Value::Int32(10)),
            constant(Value::Int32(30)),
            false,
        );
        let sel = estimate_selectivity(&pred, &stats);
        assert!(
            (sel - DEFAULT_INEQ_SEL).abs() < 1e-9,
            "BETWEEN empty histogram: expected DEFAULT_INEQ_SEL, got {}",
            sel
        );

        // NOT BETWEEN — should fall back correctly
        let pred_neg = between(
            col_ref("id"),
            constant(Value::Int32(10)),
            constant(Value::Int32(30)),
            true,
        );
        let sel_neg = estimate_selectivity(&pred_neg, &stats);
        let expected_neg = ((1.0 - 0.1) - DEFAULT_INEQ_SEL).clamp(0.0, 1.0);
        assert!(
            (sel_neg - expected_neg).abs() < 1e-9,
            "NOT BETWEEN empty histogram: expected {}, got {}",
            expected_neg,
            sel_neg
        );
    }

    #[test]
    fn test_between_single_bound_no_panic() {
        // Single histogram bound (len=1): not enough for any bucket.
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(0.0, 100.0, vec![], vec![], vec![Value::Int32(50)]),
        );
        let stats = make_stats(1000, cols);
        let pred = between(
            col_ref("id"),
            constant(Value::Int32(10)),
            constant(Value::Int32(90)),
            false,
        );
        let sel = estimate_selectivity(&pred, &stats);
        assert!(
            (sel - DEFAULT_INEQ_SEL).abs() < 1e-9,
            "BETWEEN single bound: expected DEFAULT_INEQ_SEL, got {}",
            sel
        );
    }

    // ── Regression: P1 BETWEEN fallback when comparison fails ─

    #[test]
    fn test_between_incomparable_histogram_fallback() {
        // JSON values are incomparable — histogram_fraction returns None.
        // BETWEEN should fall back to DEFAULT_INEQ_SEL, not compute 0.0.
        let mut cols = HashMap::new();
        cols.insert(
            "data".to_string(),
            make_col_stats(
                0.0,
                50.0,
                vec![],
                vec![],
                vec![
                    Value::Json("{}".to_string()),
                    Value::Json("{\"z\":1}".to_string()),
                ],
            ),
        );
        let stats = make_stats(1000, cols);
        let pred = between(
            col_ref("data"),
            constant(Value::Json("{\"a\":1}".to_string())),
            constant(Value::Json("{\"m\":1}".to_string())),
            false,
        );
        let sel = estimate_selectivity(&pred, &stats);
        assert!(
            (sel - DEFAULT_INEQ_SEL).abs() < 1e-9,
            "BETWEEN incomparable: expected DEFAULT_INEQ_SEL, got {}",
            sel
        );

        // NOT BETWEEN with incomparable types
        let pred_neg = between(
            col_ref("data"),
            constant(Value::Json("{\"a\":1}".to_string())),
            constant(Value::Json("{\"m\":1}".to_string())),
            true,
        );
        let sel_neg = estimate_selectivity(&pred_neg, &stats);
        let expected_neg = ((1.0 - DEFAULT_NULL_FRAC) - DEFAULT_INEQ_SEL).clamp(0.0, 1.0);
        assert!(
            (sel_neg - expected_neg).abs() < 1e-9,
            "NOT BETWEEN incomparable: expected {}, got {}",
            expected_neg,
            sel_neg
        );
    }

    // ── Regression: P1 NOT (NOT ...) double-negation ─────

    #[test]
    fn test_not_not_collapses() {
        // NOT (NOT (col = 1)) should equal eq_sel, not 1 - noteq_sel
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(0.2, 10.0, vec![Value::Int32(1)], vec![0.3], vec![]),
        );
        let stats = make_stats(1000, cols);

        // Direct: col = 1 → 0.3
        let eq_pred = binary_op(col_ref("id"), BinaryOp::Eq, constant(Value::Int32(1)));
        let direct_sel = estimate_selectivity(&eq_pred, &stats);

        // Double-negated: NOT (NOT (col = 1))
        let inner_not = not_expr(eq_pred.clone());
        let double_not = not_expr(inner_not);
        let double_not_sel = estimate_selectivity(&double_not, &stats);

        assert!(
            (direct_sel - double_not_sel).abs() < 1e-9,
            "NOT NOT should equal direct: direct={}, double_not={}",
            direct_sel,
            double_not_sel
        );

        // Verify direct is 0.3 as expected
        assert!(
            (direct_sel - 0.3).abs() < 1e-9,
            "eq_sel should be 0.3, got {}",
            direct_sel
        );
    }

    #[test]
    fn test_not_not_is_null() {
        // NOT (NOT (col IS NULL)) should equal IS NULL selectivity
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            make_col_stats(0.3, 10.0, vec![], vec![], vec![]),
        );
        let stats = make_stats(1000, cols);

        let is_null_pred = is_test(col_ref("id"), IsTestKind::Null, false);
        let direct_sel = estimate_selectivity(&is_null_pred, &stats);

        // NOT (NOT (IS NULL)): outer NOT unwraps to inner's inner
        let not_is_null = not_expr(is_null_pred.clone());
        let not_not_is_null = not_expr(not_is_null);
        let double_sel = estimate_selectivity(&not_not_is_null, &stats);

        assert!(
            (direct_sel - double_sel).abs() < 1e-9,
            "NOT NOT IS NULL: direct={}, double={}",
            direct_sel,
            double_sel
        );
        assert!(
            (direct_sel - 0.3).abs() < 1e-9,
            "IS NULL should be 0.3, got {}",
            direct_sel
        );
    }
}
