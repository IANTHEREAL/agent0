//! Selectivity estimation for cost-based optimization.
//!
//! Estimates the fraction of rows that satisfy a predicate, using table
//! statistics collected by ANALYZE. Only called when stats are available;
//! the physical planner falls back to legacy heuristics when no stats exist.

use crate::model::Value;
use crate::sql::analyzer::types::{BinaryOp, IsTestKind, TypedExpr, TypedExprKind, UnaryOp};
use crate::sql::expr::compare_values;
use crate::sql::optimizer::statistics::{ColumnStatistics, TableStatistics};
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
            // We don't have row_count here but eq_sel is small relative to range
            // Approximate: for <= we use the same histogram fraction since equi-depth
            // buckets treat boundary as inclusive
            range_selectivity(col_stats, value, true)
        }
        BinaryOp::Gt => range_selectivity(col_stats, value, false),
        BinaryOp::GtEq => {
            // Approximate: sel(col >= x) ≈ sel(col > x); the equality term is small
            // relative to the range fraction in equi-depth histograms.
            range_selectivity(col_stats, value, false)
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

    // Estimate fraction of values below `value` using histogram.
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

    // Binary search for first bound >= value (O(log n))
    // histogram_bounds are always sorted, so partition_point is correct.
    let pos = bounds.partition_point(|b| matches!(try_compare(b, value), Some(Ordering::Less)));

    // pos is within [1, num_buckets] due to boundary checks above.
    // bounds[pos] is the first bound that is >= value.
    match try_compare(&bounds[pos], value) {
        Some(Ordering::Equal) => {
            // Exactly at bucket boundary pos
            Some(pos as f64 / num_buckets as f64)
        }
        Some(Ordering::Greater) => {
            // Strictly inside bucket [pos-1, pos] — mid-bucket assumption
            Some((pos as f64 - 0.5) / num_buckets as f64)
        }
        _ => None, // incomparable or unexpected — fall back
    }
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
pub(super) fn n_distinct_raw(col: &ColumnStatistics, row_count: usize) -> f64 {
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

pub(super) fn get_column_stats<'a>(
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

#[cfg(test)]
mod tests;
