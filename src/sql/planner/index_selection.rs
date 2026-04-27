//! Access path selection and expression/partial index matching
//!
//! Selects the best access path (full table scan, B-tree index scan, GIN index
//! scan) for a given typed filter expression. Supports expression-index matching
//! and partial-index predicate implication.

use super::cost_model::CostModel;
use super::scan_type::{
    coerce_index_predicate_value, estimate_selectivity, extract_typed_conjuncts,
    normalize_expr_for_match, normalize_expr_string, parse_predicate_expr,
    typed_expr_to_canonical_sql,
};
use super::{AccessPath, CmpOp, ScanType, TypedPredicate};
use crate::model::{build_predicate_conjunct_cache, IndexDef, TableSchema, Value};
use crate::sql::optimizer::statistics::TableStatistics;
use crate::worker::types::IndexState;
use std::cmp::Ordering;
use std::collections::HashMap;

/// Choose the best B-tree access path for a typed filter expression.
///
/// Used by the CBO physical planner for access-path selection.
/// This is the explicit runtime-safe entry point (full-table + B-tree variants).
///
/// Covers: B-tree point lookup, range scan, bounded-range scan, in-list scan,
/// expression-index matching, and partial-index predicate implication.
pub fn choose_btree_access_path_for_typed_filter(
    schema: &TableSchema,
    filter: &crate::sql::analyzer::types::TypedExpr,
    estimated_table_rows: usize,
    table_stats: Option<&TableStatistics>,
) -> AccessPath {
    let predicates = super::predicate::analyze_typed_predicates(filter);
    choose_best_access_path_with_typed_filter(
        schema,
        &predicates,
        filter,
        estimated_table_rows,
        table_stats,
    )
}

/// Typed-filter version of access path selection.
///
/// Uses a [`TypedExpr`] filter for expression-index matching and partial-index
/// predicate implication.
fn choose_best_access_path_with_typed_filter(
    schema: &TableSchema,
    predicates: &[TypedPredicate],
    typed_filter: &crate::sql::analyzer::types::TypedExpr,
    estimated_table_rows: usize,
    table_stats: Option<&TableStatistics>,
) -> AccessPath {
    let mut best_path = AccessPath {
        scan_type: ScanType::FullTableScan,
        cost: estimated_table_rows as f64,
    };

    if let Some((scan_type, cost)) = evaluate_primary_key(schema, predicates, estimated_table_rows)
    {
        if cost < best_path.cost {
            best_path = AccessPath { scan_type, cost };
        }
    }

    for index in &schema.indexes {
        if !is_planner_usable_index(index) {
            continue;
        }

        // GIN indexes use a completely different predicate analysis path.
        if is_gin_index(index) {
            if let Some((scan_type, cost)) =
                evaluate_gin_index(schema, index, typed_filter, estimated_table_rows)
            {
                if cost < best_path.cost {
                    best_path = AccessPath { scan_type, cost };
                }
            }
            continue;
        }

        // ── B-tree path below ──────────────────────────────────
        // Partial-index predicate implication: check that the query filter
        // implies the index predicate (e.g. WHERE status = 'active' implies
        // a partial index on status = 'active').
        if index.predicate.is_some() && !query_implies_index_predicate_typed(typed_filter, index) {
            continue;
        }
        // Expression-index matching (e.g. CREATE INDEX ON t (lower(name))).
        if !index.expressions.is_empty() {
            if let Some((scan_type, cost)) =
                evaluate_expression_index_typed(index, typed_filter, estimated_table_rows)
            {
                if cost < best_path.cost {
                    best_path = AccessPath { scan_type, cost };
                }
            }
            if index.columns.is_empty() {
                continue;
            }
        }
        // Regular B-tree column index matching.
        if let Some((scan_type, cost)) =
            evaluate_index(schema, index, predicates, estimated_table_rows, table_stats)
        {
            if cost < best_path.cost {
                best_path = AccessPath { scan_type, cost };
            }
        }
    }

    best_path
}

fn primary_key_index_name(schema: &TableSchema) -> String {
    schema.pk_constraint_name.clone().unwrap_or_else(|| {
        let table_name = schema.name.rsplit('.').next().unwrap_or(&schema.name);
        format!("{}_pkey", table_name)
    })
}

fn evaluate_primary_key(
    schema: &TableSchema,
    predicates: &[TypedPredicate],
    estimated_table_rows: usize,
) -> Option<(ScanType, f64)> {
    if schema.pk_indices.is_empty() {
        return None;
    }

    let eq_predicate_map = eq_predicates_by_column_index(schema, predicates);

    let mut values = Vec::with_capacity(schema.pk_indices.len());
    for &col_idx in &schema.pk_indices {
        let col = &schema.columns[col_idx].name;
        let Some(val) = eq_predicate_map.get(&col_idx) else {
            break;
        };
        values.push(coerce_index_predicate_value(schema, col, val));
    }

    if values.is_empty() {
        return None;
    }

    if values.len() < schema.pk_indices.len() {
        let selectivity = CostModel::NON_UNIQUE_SELECTIVITY_BASE.powi(values.len() as i32);
        let estimated_rows = ((estimated_table_rows as f64) * selectivity).max(1.0) as usize;
        let cost = index_scan_cost(estimated_rows, estimated_table_rows);
        return Some((
            ScanType::PrimaryKeyRangeScan {
                index_name: primary_key_index_name(schema),
                prefix_values: values,
            },
            cost,
        ));
    }

    let cost = index_scan_cost(1, estimated_table_rows);
    Some((
        ScanType::PrimaryKeyScan {
            index_name: primary_key_index_name(schema),
            values,
        },
        cost,
    ))
}

fn is_planner_usable_index(index: &IndexDef) -> bool {
    if index.state != IndexState::Ready {
        return false;
    }
    if index.columns.is_empty() && index.expressions.is_empty() {
        return false;
    }
    let method = index.method.as_deref().unwrap_or("btree");
    method.eq_ignore_ascii_case("btree") || method.eq_ignore_ascii_case("gin")
}

fn predicate_matches_column(
    schema: &TableSchema,
    predicate_column: &str,
    predicate_column_index: usize,
    target_column_index: usize,
) -> bool {
    let Some(target_column) = schema.columns.get(target_column_index) else {
        return false;
    };

    if schema.columns.get(predicate_column_index).is_some() {
        return predicate_column_index == target_column_index
            && target_column.name == predicate_column;
    }

    // Unit tests and a few synthetic optimizer paths may construct TypedExprs
    // without analyzer-accurate column_index values. Only allow exact-name
    // fallback when the index is invalid; a valid-but-mismatched index/name pair
    // is ambiguous and must not drive an over-restrictive access path.
    target_column.name == predicate_column
}

fn eq_predicates_by_column_index<'a>(
    schema: &TableSchema,
    predicates: &'a [TypedPredicate],
) -> HashMap<usize, &'a Value> {
    let mut out = HashMap::new();
    for predicate in predicates {
        if let TypedPredicate::Comparison {
            column,
            column_index,
            op: CmpOp::Eq,
            value,
        } = predicate
        {
            for (target_column_index, _) in schema.columns.iter().enumerate() {
                if predicate_matches_column(schema, column, *column_index, target_column_index) {
                    out.entry(target_column_index).or_insert(value);
                    break;
                }
            }
        }
    }
    out
}

/// Returns true if this index uses the GIN access method.
fn is_gin_index(index: &IndexDef) -> bool {
    index
        .method
        .as_deref()
        .is_some_and(|m| m.eq_ignore_ascii_case("gin"))
}

/// Evaluate whether a GIN index can accelerate the given filter.
///
/// Delegates to `gin_predicate::try_extract_gin_predicate` for predicate
/// analysis, then computes cost using GIN-specific cost model constants.
fn evaluate_gin_index(
    schema: &TableSchema,
    index: &IndexDef,
    filter: &crate::sql::analyzer::types::TypedExpr,
    estimated_table_rows: usize,
) -> Option<(ScanType, f64)> {
    let m = super::gin_predicate::try_extract_gin_predicate(schema, index, filter)?;

    // Reject pure-negative quals (e.g. `NOT 'foo'`) — these cannot use the
    // inverted index because there is no positive term to scan.
    if !super::gin_predicate::gin_qual_has_positive_term(&m.qual) {
        return None;
    }

    let n_tokens = super::gin_predicate::gin_qual_term_count(&m.qual);
    let selectivity = CostModel::GIN_DEFAULT_SELECTIVITY;
    let estimated_rows = ((estimated_table_rows as f64) * selectivity).max(1.0) as usize;

    let cost = CostModel::GIN_SCAN_BASE_COST
        + n_tokens as f64 * CostModel::GIN_TOKEN_SCAN_COST
        + estimated_rows as f64 * CostModel::GIN_ROW_FETCH_COST;

    Some((
        ScanType::GinIndexScan {
            index_id: m.index_id,
            index_name: m.index_name,
            qual: m.qual,
        },
        cost,
    ))
}
/// Compute index scan cost from estimated matching rows.
///
/// When the index would fetch more than [`CostModel::FULL_SCAN_SELECTIVITY_THRESHOLD`]
/// of the table, the per-row cost is raised to [`CostModel::RANDOM_IO_COST_PER_ROW`]
/// to reflect random I/O overhead, causing the planner to prefer a full table scan.
fn index_scan_cost(estimated_rows: usize, estimated_table_rows: usize) -> f64 {
    let selectivity = if estimated_table_rows > 0 {
        estimated_rows as f64 / estimated_table_rows as f64
    } else {
        0.0
    };
    let row_cost = if selectivity > CostModel::FULL_SCAN_SELECTIVITY_THRESHOLD {
        CostModel::RANDOM_IO_COST_PER_ROW
    } else {
        CostModel::ROW_COST
    };
    CostModel::BASE_COST + estimated_rows as f64 * row_cost
}

pub(super) fn compute_inlist_selectivity(
    in_values: &[Value],
    index: &IndexDef,
    prefix_values: &[Value],
    table_rows: usize,
    table_stats: Option<&TableStatistics>,
    _schema: &TableSchema,
) -> f64 {
    let next_col_pos = prefix_values.len(); // 0-indexed position of InList column

    // `in_values` must already be normalized to distinct non-NULL values.
    let effective_in_count = in_values.len() as f64;
    if effective_in_count == 0.0 {
        return CostModel::MIN_SELECTIVITY_FLOOR;
    }

    let table_rows_f = table_rows.max(1) as f64;

    // Branch 1: unique index with full prefix coverage -> unique lookup semantics
    let full_key_len = index.columns.len();
    if index.unique && next_col_pos + 1 == full_key_len {
        return (effective_in_count / table_rows_f).clamp(0.0, 1.0);
    }

    // Branch 2: NDV available for this column
    if let Some(stats) = table_stats {
        if let Some((_, column)) = stats
            .columns
            .iter()
            .find(|(col_name, _)| col_name.eq_ignore_ascii_case(&index.columns[next_col_pos]))
        {
            let ndv = if column.n_distinct > 0.0 {
                column.n_distinct
            } else {
                (-column.n_distinct * table_rows_f).max(1.0)
            };
            let non_null_frac = (1.0 - column.null_fraction).max(0.0);
            let inlist_sel = (effective_in_count / ndv) * non_null_frac;

            // Factor in prefix equality selectivity for composite indexes.
            // Without this, a query like `tenant_id = ? AND status IN (...)`
            // on index (tenant_id, status) would estimate selectivity based
            // only on the status column, ignoring that tenant_id already
            // narrows the result set significantly.
            let prefix_sel =
                compute_prefix_equality_selectivity(index, next_col_pos, table_rows_f, stats);
            let sel = prefix_sel * inlist_sel;

            return sel.clamp(CostModel::MIN_SELECTIVITY_FLOOR, 1.0);
        }
    }

    // Branch 3: no stats -> existing matched-column heuristic including InList column
    let base = estimate_selectivity(index, next_col_pos + 1, false);
    (effective_in_count * base).clamp(CostModel::MIN_SELECTIVITY_FLOOR, 1.0)
}

/// Compute combined selectivity for prefix equality columns using per-column NDV.
///
/// For each prefix column with available statistics, uses `1/ndv * non_null_frac`.
/// Falls back to `NON_UNIQUE_SELECTIVITY_BASE` for columns without stats.
fn compute_prefix_equality_selectivity(
    index: &IndexDef,
    prefix_len: usize,
    table_rows_f: f64,
    stats: &crate::sql::optimizer::statistics::TableStatistics,
) -> f64 {
    if prefix_len == 0 {
        return 1.0;
    }
    let mut sel = 1.0;
    for i in 0..prefix_len {
        let col_name = &index.columns[i];
        if let Some((_, column)) = stats
            .columns
            .iter()
            .find(|(cn, _)| cn.eq_ignore_ascii_case(col_name))
        {
            let ndv = if column.n_distinct > 0.0 {
                column.n_distinct
            } else {
                (-column.n_distinct * table_rows_f).max(1.0)
            };
            let non_null_frac = (1.0 - column.null_fraction).max(0.0);
            sel *= (1.0 / ndv) * non_null_frac;
        } else {
            sel *= CostModel::NON_UNIQUE_SELECTIVITY_BASE;
        }
    }
    sel
}

fn normalize_inlist_values(in_values: &[Value]) -> Vec<Value> {
    let mut normalized: Vec<Value> = in_values
        .iter()
        .filter(|value| !matches!(value, Value::Null))
        .cloned()
        .collect();

    // Canonicalize all NaN encodings before sort+dedup so mixed-sign NaNs
    // become adjacent and collapse to one effective value.
    for value in &mut normalized {
        if let Value::Float64(f) = value {
            if f.is_nan() {
                *f = f64::NAN;
            }
        }
    }

    normalized.sort_by(value_total_cmp);
    normalized.dedup_by(|a, b| value_nan_aware_eq(a, b));
    normalized
}

fn value_nan_aware_eq(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Float64(l), Value::Float64(r)) => (l.is_nan() && r.is_nan()) || l == r,
        _ => left == right,
    }
}

fn value_total_cmp(left: &Value, right: &Value) -> Ordering {
    match (left, right) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Boolean(l), Value::Boolean(r)) => l.cmp(r),
        (Value::Int32(l), Value::Int32(r)) => l.cmp(r),
        (Value::Int64(l), Value::Int64(r)) => l.cmp(r),
        (Value::Float64(l), Value::Float64(r)) => l.total_cmp(r),
        (Value::Text(l), Value::Text(r)) => l.cmp(r),
        (Value::Bytes(l), Value::Bytes(r)) => l.cmp(r),
        (Value::Timestamp(l), Value::Timestamp(r)) => l.cmp(r),
        (Value::Interval(l), Value::Interval(r)) => {
            let by_months = l.months.cmp(&r.months);
            if by_months == Ordering::Equal {
                l.millis.cmp(&r.millis)
            } else {
                by_months
            }
        }
        (Value::Uuid(l), Value::Uuid(r)) => l.cmp(r),
        (Value::Array(l), Value::Array(r)) => {
            for (lv, rv) in l.iter().zip(r.iter()) {
                let ord = value_total_cmp(lv, rv);
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            l.len().cmp(&r.len())
        }
        (Value::Vector(l), Value::Vector(r)) => {
            for (lv, rv) in l.iter().zip(r.iter()) {
                let ord = lv.total_cmp(rv);
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            l.len().cmp(&r.len())
        }
        (Value::Json(l), Value::Json(r)) => l.cmp(r),
        (Value::Jsonb(l), Value::Jsonb(r)) => l.cmp(r),
        (Value::Time(l), Value::Time(r)) => l.cmp(r),
        (Value::Date(l), Value::Date(r)) => l.cmp(r),
        (Value::Numeric(l), Value::Numeric(r)) => l.cmp(r),
        (Value::Tsvector(l), Value::Tsvector(r)) => l.cmp(r),
        (Value::Tsquery(l), Value::Tsquery(r)) => l.cmp(r),
        _ => value_type_rank(left).cmp(&value_type_rank(right)),
    }
}

fn value_type_rank(value: &Value) -> u8 {
    match value {
        Value::Null => 0,
        Value::Boolean(_) => 1,
        Value::Int32(_) => 2,
        Value::Int64(_) => 3,
        Value::Float64(_) => 4,
        Value::Text(_) => 5,
        Value::Bytes(_) => 6,
        Value::Timestamp(_) => 7,
        Value::Interval(_) => 8,
        Value::Uuid(_) => 9,
        Value::Array(_) => 10,
        Value::Vector(_) => 11,
        Value::Json(_) => 12,
        Value::Jsonb(_) => 13,
        Value::Time(_) => 14,
        Value::Date(_) => 15,
        Value::Numeric(_) => 16,
        Value::Tsvector(_) => 17,
        Value::Tsquery(_) => 18,
    }
}

fn evaluate_index(
    schema: &TableSchema,
    index: &IndexDef,
    predicates: &[TypedPredicate],
    estimated_table_rows: usize,
    table_stats: Option<&TableStatistics>,
) -> Option<(ScanType, f64)> {
    let index_column_indices: Vec<usize> = index
        .columns
        .iter()
        .map(|column| schema.column_index(column))
        .collect::<Option<Vec<_>>>()?;

    // Build a map of column index → constant value for Eq predicates.
    let eq_predicate_map = eq_predicates_by_column_index(schema, predicates);

    let mut prefix_values = Vec::new();

    for (col, col_idx) in index.columns.iter().zip(index_column_indices.iter()) {
        if let Some(val) = eq_predicate_map.get(col_idx) {
            prefix_values.push(coerce_index_predicate_value(schema, col, val));
        } else {
            break;
        }
    }

    if prefix_values.len() == index.columns.len() {
        let selectivity = estimate_selectivity(index, prefix_values.len(), true);
        let estimated_rows = ((estimated_table_rows as f64) * selectivity).max(1.0) as usize;
        let cost = index_scan_cost(estimated_rows, estimated_table_rows);

        return Some((
            ScanType::IndexScan {
                index_id: index.id,
                index_name: index.name.clone(),
                lookup_column: (index.columns.len() == 1 && index.expressions.is_empty())
                    .then(|| index.columns[0].clone()),
                values: prefix_values,
            },
            cost,
        ));
    }

    let next_col = index.columns.get(prefix_values.len())?;
    let next_col_idx = index_column_indices[prefix_values.len()];

    if let Some(in_values) = predicates.iter().find_map(|p| {
        if let TypedPredicate::InList {
            column,
            column_index,
            values,
        } = p
        {
            if predicate_matches_column(schema, column, *column_index, next_col_idx)
                && !values.is_empty()
            {
                return Some(values);
            }
        }
        None
    }) {
        let normalized_in_values = normalize_inlist_values(in_values);
        let mut column_values = Vec::with_capacity(normalized_in_values.len());
        for in_value in &normalized_in_values {
            let mut lookup = prefix_values.clone();
            lookup.push(coerce_index_predicate_value(schema, next_col, in_value));
            column_values.push(lookup);
        }
        let selectivity = compute_inlist_selectivity(
            &normalized_in_values,
            index,
            &prefix_values,
            estimated_table_rows,
            table_stats,
            schema,
        );
        let estimated_rows = ((estimated_table_rows as f64) * selectivity).max(1.0) as usize;
        let cost = index_scan_cost(estimated_rows, estimated_table_rows);

        return Some((
            ScanType::InListScan {
                index_id: index.id,
                index_name: index.name.clone(),
                lookup_column: Some(next_col.clone()),
                column_values,
            },
            cost,
        ));
    }

    // Extract all range bounds in a single pass (was 4 separate O(N) scans).
    let mut lower_inclusive = None;
    let mut lower_exclusive = None;
    let mut upper_inclusive = None;
    let mut upper_exclusive = None;
    for p in predicates {
        if let TypedPredicate::Comparison {
            column,
            column_index,
            op,
            value,
        } = p
        {
            if predicate_matches_column(schema, column, *column_index, next_col_idx) {
                match op {
                    CmpOp::Ge => lower_inclusive = lower_inclusive.or(Some(value)),
                    CmpOp::Gt => lower_exclusive = lower_exclusive.or(Some(value)),
                    CmpOp::Le => upper_inclusive = upper_inclusive.or(Some(value)),
                    CmpOp::Lt => upper_exclusive = upper_exclusive.or(Some(value)),
                    _ => {}
                }
            }
        }
    }

    let (range_start, start_inclusive) = if let Some(val) = lower_inclusive {
        (
            Some(coerce_index_predicate_value(schema, next_col, val)),
            true,
        )
    } else if let Some(val) = lower_exclusive {
        (
            Some(coerce_index_predicate_value(schema, next_col, val)),
            false,
        )
    } else {
        (None, true)
    };

    let (range_end, end_inclusive) = if let Some(val) = upper_inclusive {
        (
            Some(coerce_index_predicate_value(schema, next_col, val)),
            true,
        )
    } else if let Some(val) = upper_exclusive {
        (
            Some(coerce_index_predicate_value(schema, next_col, val)),
            false,
        )
    } else {
        (None, true)
    };

    if range_start.is_some() || range_end.is_some() {
        let two_sided = range_start.is_some() && range_end.is_some();
        let selectivity = if two_sided {
            CostModel::TWO_SIDED_RANGE_SELECTIVITY
        } else {
            CostModel::ONE_SIDED_RANGE_SELECTIVITY
        };
        let estimated_rows = ((estimated_table_rows as f64) * selectivity).max(1.0) as usize;
        let cost = index_scan_cost(estimated_rows, estimated_table_rows);

        return Some((
            ScanType::IndexBoundedRangeScan {
                index_id: index.id,
                index_name: index.name.clone(),
                prefix_values,
                range_start,
                start_inclusive,
                range_end,
                end_inclusive,
            },
            cost,
        ));
    }

    if prefix_values.is_empty() {
        return None;
    }

    let selectivity = estimate_selectivity(index, prefix_values.len(), false);
    let estimated_rows = ((estimated_table_rows as f64) * selectivity).max(1.0) as usize;
    let cost = index_scan_cost(estimated_rows, estimated_table_rows);

    Some((
        ScanType::IndexRangeScan {
            index_id: index.id,
            index_name: index.name.clone(),
            prefix_values,
        },
        cost,
    ))
}

/// Check if a typed query filter implies a partial-index predicate.
///
/// The index predicate is stored as a string, so we parse it to AST and normalize.
/// The query conjuncts come from the TypedExpr tree via
/// [`typed_expr_to_canonical_sql`] + [`normalize_expr_string`].
fn query_implies_index_predicate_typed(
    filter: &crate::sql::analyzer::types::TypedExpr,
    index: &IndexDef,
) -> bool {
    let index_conjuncts = if let Some(cached) = index.cached_predicate_conjuncts.as_ref() {
        cached.clone()
    } else if let Some(parsed) = build_predicate_conjunct_cache(index.predicate.as_deref()) {
        parsed
    } else {
        return false;
    };

    let query_conjuncts: Vec<String> = extract_typed_conjuncts(filter)
        .iter()
        .map(|c| normalize_expr_string(typed_expr_to_canonical_sql(c)))
        .collect();

    index_conjuncts
        .iter()
        .all(|idx_conj| query_conjuncts.iter().any(|q| q == idx_conj))
}

/// Evaluate expression-index applicability using a typed filter.
///
/// For each expression in the index, check if any typed filter conjunct
/// of the form `expr = constant` matches (after canonical normalization).
fn evaluate_expression_index_typed(
    index: &IndexDef,
    filter: &crate::sql::analyzer::types::TypedExpr,
    estimated_table_rows: usize,
) -> Option<(ScanType, f64)> {
    let values = match_expression_predicates_typed(index, filter)?;
    let selectivity = estimate_selectivity(index, values.len(), true);
    let estimated_rows = ((estimated_table_rows as f64) * selectivity).max(1.0) as usize;
    let cost = index_scan_cost(estimated_rows, estimated_table_rows);

    Some((
        ScanType::IndexScan {
            index_id: index.id,
            index_name: index.name.clone(),
            lookup_column: None,
            values,
        },
        cost,
    ))
}

/// Match expression-index expressions against typed filter conjuncts.
///
/// For each index expression string:
/// 1. Parse to AST -> `normalize_expr_for_match` (the "index side")
/// 2. For each typed filter conjunct of the form `lhs = rhs`:
///    - Canonicalize lhs/rhs via `typed_expr_to_canonical_sql` -> `normalize_expr_string`
///    - If one side matches the index expression, the other must be a constant value
fn match_expression_predicates_typed(
    index: &IndexDef,
    filter: &crate::sql::analyzer::types::TypedExpr,
) -> Option<Vec<Value>> {
    use crate::sql::analyzer::types::{BinaryOp as TypedBinaryOp, TypedExprKind};

    if index.expressions.is_empty() {
        return None;
    }

    let filter_conjuncts = extract_typed_conjuncts(filter);
    let mut values = Vec::with_capacity(index.expressions.len());

    for expr_str in &index.expressions {
        let expr_ast = parse_predicate_expr(expr_str)?;
        let normalized_expr = normalize_expr_for_match(&expr_ast);
        let mut matched_value = None;

        for conjunct in &filter_conjuncts {
            if let TypedExprKind::BinaryOp {
                left,
                op: TypedBinaryOp::Eq,
                right,
            } = &conjunct.kind
            {
                let left_norm = normalize_expr_string(typed_expr_to_canonical_sql(left));
                let right_norm = normalize_expr_string(typed_expr_to_canonical_sql(right));

                if left_norm == normalized_expr {
                    if let TypedExprKind::Constant(v) = &right.kind {
                        matched_value = Some(v.clone());
                        break;
                    }
                }
                if right_norm == normalized_expr {
                    if let TypedExprKind::Constant(v) = &left.kind {
                        matched_value = Some(v.clone());
                        break;
                    }
                }
            }
        }

        values.push(matched_value?);
    }

    Some(values)
}
