//! Physical query planner that builds operator trees from SQL AST
//!
//! This module bridges the gap between the SQL parser and the physical operators.
//! It takes a parsed SELECT query and produces an operator tree that can be executed.
//!
//! # Supported Query Patterns
//!
//! Currently supports simple SELECT queries:
//! - Single table (no JOINs)
//! - Optional WHERE clause (with index selection)
//! - Optional ORDER BY
//! - Optional LIMIT/OFFSET
//!
//! # Example
//!
//! ```ignore
//! let planner = PhysicalPlanner::new(search_path);
//! let operator = planner.plan_simple_select(db_id, schema, filter, order_by, limit, offset, 1000)?;
//! ```

use anyhow::Result;
use sqlparser::ast::{BinaryOperator, Expr, OrderByExpr};
use std::collections::HashMap;

use super::{
    BoxedOperator, FilterOperator, InListScanOperator, IndexScanOperator, LimitOperator,
    RangeIndexScanOperator, SortOperator, TableScanOperator,
};
use crate::sql::expr::eval_expr;
use crate::sql::planner::{choose_best_access_path_for_filter, ScanType};
use crate::sql::value_coercion::coerce_value_for_column;
use crate::types::{TableSchema, Value};

fn extract_column_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Identifier(ident) => Some(ident.value.to_lowercase()),
        Expr::CompoundIdentifier(parts) => parts.last().map(|i| i.value.to_lowercase()),
        Expr::Nested(inner) => extract_column_name(inner),
        _ => None,
    }
}

fn collect_eq_predicates(expr: &Expr, out: &mut HashMap<String, Value>) -> Option<()> {
    match expr {
        Expr::Nested(inner) => collect_eq_predicates(inner, out),
        Expr::BinaryOp { left, op, right } => match op {
            BinaryOperator::And => {
                collect_eq_predicates(left, out)?;
                collect_eq_predicates(right, out)?;
                Some(())
            }
            BinaryOperator::Eq => {
                let (col, val) = if let Some(col) = extract_column_name(left) {
                    (col, eval_expr(right, None, None).ok()?)
                } else if let Some(col) = extract_column_name(right) {
                    (col, eval_expr(left, None, None).ok()?)
                } else {
                    return None;
                };

                if let Some(existing) = out.get(&col) {
                    if existing != &val {
                        return None;
                    }
                    return Some(());
                }

                out.insert(col, val);
                Some(())
            }
            _ => None,
        },
        _ => None,
    }
}

fn filter_is_exact_index_lookup(
    filter: &Expr,
    schema: &TableSchema,
    index_id: u64,
    lookup_values: &[Value],
) -> bool {
    let Some(index) = schema.indexes.iter().find(|i| i.id == index_id) else {
        return false;
    };

    if lookup_values.is_empty() || lookup_values.len() > index.columns.len() {
        return false;
    }

    let mut predicates: HashMap<String, Value> = HashMap::new();
    if collect_eq_predicates(filter, &mut predicates).is_none() {
        return false;
    }

    if predicates.len() != lookup_values.len() {
        return false;
    }

    for (i, col) in index.columns.iter().take(lookup_values.len()).enumerate() {
        let key = col.to_lowercase();
        let Some(pred_value) = predicates.get(&key) else {
            return false;
        };

        let coerced = if let Some(col_def) = schema
            .columns
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(col))
        {
            coerce_value_for_column(pred_value.clone(), col_def)
                .unwrap_or_else(|_| pred_value.clone())
        } else {
            pred_value.clone()
        };

        if coerced != lookup_values[i] {
            return false;
        }
    }

    true
}

/// Physical query planner that builds operator trees.
///
/// The planner uses cost-based optimization to choose access paths (full scan vs index scan)
/// and constructs a tree of physical operators that implement the Volcano iterator model.
pub struct PhysicalPlanner {
    search_path: Vec<String>,
}

impl std::fmt::Debug for PhysicalPlanner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhysicalPlanner")
            .field("search_path", &self.search_path)
            .finish_non_exhaustive()
    }
}

impl PhysicalPlanner {
    /// Create a new physical planner.
    pub fn new(search_path: Vec<String>) -> Self {
        Self { search_path }
    }

    /// Plan a simple SELECT query (single table, no JOINs).
    ///
    /// This builds an operator tree for queries of the form:
    /// ```sql
    /// SELECT * FROM table WHERE filter ORDER BY cols LIMIT n OFFSET m
    /// ```
    ///
    /// The operator tree is built bottom-up:
    /// 1. Scan operator (TableScan or IndexScan based on cost)
    /// 2. Filter operator (if WHERE clause present and not fully handled by index)
    /// 3. Sort operator (if ORDER BY present)
    /// 4. Limit operator (if LIMIT/OFFSET present)
    ///
    /// # Arguments
    ///
    /// * `schema` - Table schema
    /// * `filter` - Optional WHERE clause expression
    /// * `order_by` - ORDER BY expressions (can be empty)
    /// * `limit` - Optional LIMIT value
    /// * `offset` - OFFSET value (0 if not specified)
    /// * `estimated_rows` - Estimated table row count for cost estimation
    ///
    /// # Returns
    ///
    /// A boxed operator that is the root of the execution tree.
    pub fn plan_simple_select(
        &self,
        db_id: u64,
        schema: TableSchema,
        filter: Option<&Expr>,
        order_by: Vec<OrderByExpr>,
        limit: Option<usize>,
        offset: usize,
        estimated_rows: usize,
    ) -> Result<BoxedOperator> {
        let access_path =
            choose_best_access_path_for_filter(db_id, &schema, filter, estimated_rows);

        let scan_upper_bound = match limit {
            Some(0) => Some(0),
            Some(n) => Some(offset.saturating_add(n)),
            None => None,
        };

        let mut root: BoxedOperator = match access_path.scan_type {
            ScanType::FullTableScan => {
                let scan_limit = if filter.is_none() && order_by.is_empty() {
                    scan_upper_bound
                } else {
                    None
                };
                Box::new(TableScanOperator::new_with_scan_limit(schema, scan_limit))
            }
            ScanType::IndexScan {
                index_id,
                index_name,
                values,
                ..
            } => {
                let scan_limit = if order_by.is_empty()
                    && scan_upper_bound.is_some()
                    && filter.is_some_and(|f| {
                        filter_is_exact_index_lookup(f, &schema, index_id, &values)
                    }) {
                    scan_upper_bound
                } else {
                    None
                };
                Box::new(IndexScanOperator::new_with_scan_limit(
                    schema, index_id, index_name, values, scan_limit,
                ))
            }
            ScanType::IndexRangeScan {
                index_id,
                index_name,
                prefix_values,
                ..
            } => {
                let scan_limit = if order_by.is_empty()
                    && scan_upper_bound.is_some()
                    && filter.is_some_and(|f| {
                        filter_is_exact_index_lookup(f, &schema, index_id, &prefix_values)
                    }) {
                    scan_upper_bound
                } else {
                    None
                };
                Box::new(IndexScanOperator::new_with_scan_limit(
                    schema,
                    index_id,
                    index_name,
                    prefix_values,
                    scan_limit,
                ))
            }
            ScanType::IndexBoundedRangeScan {
                index_id,
                index_name,
                prefix_values,
                range_start,
                start_inclusive,
                range_end,
                end_inclusive,
                ..
            } => Box::new(RangeIndexScanOperator::new(
                schema,
                index_id,
                index_name,
                prefix_values,
                range_start,
                start_inclusive,
                range_end,
                end_inclusive,
            )),
            ScanType::InListScan {
                index_id,
                index_name,
                column_values,
                ..
            } => Box::new(InListScanOperator::new(
                schema,
                index_id,
                index_name,
                column_values,
            )),
            ScanType::GinIndexScan { .. } => Box::new(TableScanOperator::new(schema)),
        };

        if let Some(filter_expr) = filter {
            root = Box::new(FilterOperator::new(root, filter_expr.clone()));
        }

        if !order_by.is_empty() {
            root = Box::new(SortOperator::new(root, order_by));
        }

        if limit.is_some() || offset > 0 {
            root = Box::new(LimitOperator::new(root, limit, offset));
        }

        Ok(root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::ast::{BinaryOperator, Ident};

    fn make_eq_expr(col: &str, val: i32) -> Expr {
        Expr::BinaryOp {
            left: Box::new(Expr::Identifier(Ident::new(col))),
            op: BinaryOperator::Eq,
            right: Box::new(Expr::Value(sqlparser::ast::Value::Number(
                val.to_string(),
                false,
            ))),
        }
    }

    #[test]
    fn test_collect_eq_predicates_single() {
        let expr = make_eq_expr("id", 42);
        let mut out = HashMap::new();
        assert!(super::collect_eq_predicates(&expr, &mut out).is_some());
        assert_eq!(out.len(), 1);
        assert!(matches!(
            out.get("id"),
            Some(Value::Int32(42)) | Some(Value::Int64(42))
        ));
    }

    #[test]
    fn test_collect_eq_predicates_and_conjunction() {
        let expr = Expr::BinaryOp {
            left: Box::new(make_eq_expr("a", 1)),
            op: BinaryOperator::And,
            right: Box::new(make_eq_expr("b", 2)),
        };
        let mut out = HashMap::new();
        assert!(super::collect_eq_predicates(&expr, &mut out).is_some());
        assert_eq!(out.len(), 2);
        assert!(out.contains_key("a"));
        assert!(out.contains_key("b"));
    }

    #[test]
    fn test_collect_eq_predicates_non_eq_returns_none() {
        let expr = Expr::BinaryOp {
            left: Box::new(Expr::Identifier(Ident::new("id"))),
            op: BinaryOperator::Gt,
            right: Box::new(Expr::Value(sqlparser::ast::Value::Number(
                "5".to_string(),
                false,
            ))),
        };
        let mut out = HashMap::new();
        assert!(super::collect_eq_predicates(&expr, &mut out).is_none());
        assert!(out.is_empty());
    }

    #[test]
    fn test_collect_eq_predicates_conflicting_values() {
        let expr = Expr::BinaryOp {
            left: Box::new(make_eq_expr("id", 1)),
            op: BinaryOperator::And,
            right: Box::new(make_eq_expr("id", 2)),
        };
        let mut out = HashMap::new();
        assert!(super::collect_eq_predicates(&expr, &mut out).is_none());
    }
}
