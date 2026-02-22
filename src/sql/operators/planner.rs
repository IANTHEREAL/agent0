//! Physical query planner that builds operator trees
//!
//! This module bridges the gap between the Analyzer's typed IR and the physical operators.
//! It takes typed expressions and produces an operator tree that can be executed.
//!
//! # Supported Query Patterns
//!
//! Currently supports simple SELECT queries:
//! - Single table (no JOINs)
//! - Optional WHERE clause (with index selection)
//! - Optional ORDER BY
//! - Optional LIMIT/OFFSET

use anyhow::{anyhow, Result};
use std::collections::HashMap;

use crate::sql::analyzer::types::{TypedExpr, TypedOrderByExpr};

use super::{
    BoxedOperator, FilterOperator, InListScanOperator, IndexScanOperator, LimitOperator,
    RangeIndexScanOperator, SortOperator, TableScanOperator,
};
use crate::sql::planner::{
    choose_best_access_path_for_typed_filter, collect_typed_eq_predicates, ScanType,
};
use crate::sql::value_coercion::coerce_value_for_column;
use crate::types::{TableSchema, Value};

fn filter_is_exact_index_lookup(
    filter: &TypedExpr,
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
    if collect_typed_eq_predicates(filter, &mut predicates).is_none() {
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
    pub fn plan_simple_select(
        &self,
        db_id: u64,
        schema: TableSchema,
        filter: Option<&TypedExpr>,
        order_by: Vec<TypedOrderByExpr>,
        limit: Option<usize>,
        offset: usize,
        estimated_rows: usize,
    ) -> Result<BoxedOperator> {
        let access_path =
            choose_best_access_path_for_typed_filter(db_id, &schema, filter, estimated_rows);

        let scan_upper_bound = match limit {
            Some(0) => Some(0),
            Some(n) => Some(offset.saturating_add(n)),
            None => None,
        };

        let mut root: BoxedOperator = build_scan_root_from_scan_type(
            schema,
            access_path.scan_type,
            scan_upper_bound,
            filter,
            &order_by,
        )?;

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

fn build_scan_root_from_scan_type(
    schema: TableSchema,
    scan_type: ScanType,
    scan_upper_bound: Option<usize>,
    filter: Option<&TypedExpr>,
    order_by: &[TypedOrderByExpr],
) -> Result<BoxedOperator> {
    let table_name = schema.name.clone();
    match scan_type {
        ScanType::FullTableScan => {
            let scan_limit = if filter.is_none() && order_by.is_empty() {
                scan_upper_bound
            } else {
                None
            };
            Ok(Box::new(TableScanOperator::new_with_scan_limit(
                schema, scan_limit,
            )))
        }
        ScanType::IndexScan {
            index_id,
            index_name,
            values,
            ..
        } => {
            let scan_limit = if order_by.is_empty()
                && scan_upper_bound.is_some()
                && filter
                    .is_some_and(|f| filter_is_exact_index_lookup(f, &schema, index_id, &values))
            {
                scan_upper_bound
            } else {
                None
            };
            Ok(Box::new(IndexScanOperator::new_with_scan_limit(
                schema, index_id, index_name, values, scan_limit,
            )))
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
            Ok(Box::new(IndexScanOperator::new_with_scan_limit(
                schema,
                index_id,
                index_name,
                prefix_values,
                scan_limit,
            )))
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
        } => Ok(Box::new(RangeIndexScanOperator::new(
            schema,
            index_id,
            index_name,
            prefix_values,
            range_start,
            start_inclusive,
            range_end,
            end_inclusive,
        ))),
        ScanType::InListScan {
            index_id,
            index_name,
            column_values,
            ..
        } => Ok(Box::new(InListScanOperator::new(
            schema,
            index_id,
            index_name,
            column_values,
        ))),
        ScanType::GinIndexScan { .. } => Err(anyhow!(
            "Unexpected ScanType::GinIndexScan in operators::planner for table '{}'",
            table_name
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::analyzer::types::{BinaryOp as TypedBinaryOp, TypedExpr, TypedExprKind};
    use crate::types::{ColumnDef, DataType};

    fn make_typed_eq_expr(col: &str, col_idx: usize, val: i32) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(TypedExpr {
                    kind: TypedExprKind::ColumnRef {
                        scope_depth: 0,
                        column_index: col_idx,
                        column_name: col.to_string(),
                    },
                    data_type: DataType::Int32,
                }),
                op: TypedBinaryOp::Eq,
                right: Box::new(TypedExpr {
                    kind: TypedExprKind::Constant(Value::Int32(val)),
                    data_type: DataType::Int32,
                }),
            },
            data_type: DataType::Boolean,
        }
    }

    #[test]
    fn test_collect_eq_predicates_single() {
        let expr = make_typed_eq_expr("id", 0, 42);
        let mut out = HashMap::new();
        assert!(super::collect_typed_eq_predicates(&expr, &mut out).is_some());
        assert_eq!(out.len(), 1);
        assert_eq!(out.get("id"), Some(&Value::Int32(42)));
    }

    #[test]
    fn test_collect_eq_predicates_and_conjunction() {
        let expr = TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(make_typed_eq_expr("a", 0, 1)),
                op: TypedBinaryOp::And,
                right: Box::new(make_typed_eq_expr("b", 1, 2)),
            },
            data_type: DataType::Boolean,
        };
        let mut out = HashMap::new();
        assert!(super::collect_typed_eq_predicates(&expr, &mut out).is_some());
        assert_eq!(out.len(), 2);
        assert!(out.contains_key("a"));
        assert!(out.contains_key("b"));
    }

    #[test]
    fn test_collect_eq_predicates_non_eq_returns_none() {
        let expr = TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(TypedExpr {
                    kind: TypedExprKind::ColumnRef {
                        scope_depth: 0,
                        column_index: 0,
                        column_name: "id".to_string(),
                    },
                    data_type: DataType::Int32,
                }),
                op: TypedBinaryOp::Gt,
                right: Box::new(TypedExpr {
                    kind: TypedExprKind::Constant(Value::Int32(5)),
                    data_type: DataType::Int32,
                }),
            },
            data_type: DataType::Boolean,
        };
        let mut out = HashMap::new();
        assert!(super::collect_typed_eq_predicates(&expr, &mut out).is_none());
        assert!(out.is_empty());
    }

    #[test]
    fn test_collect_eq_predicates_conflicting_values() {
        let expr = TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(make_typed_eq_expr("id", 0, 1)),
                op: TypedBinaryOp::And,
                right: Box::new(make_typed_eq_expr("id", 0, 2)),
            },
            data_type: DataType::Boolean,
        };
        let mut out = HashMap::new();
        assert!(super::collect_typed_eq_predicates(&expr, &mut out).is_none());
    }

    fn planner_test_schema() -> TableSchema {
        TableSchema::new(
            "t".to_string(),
            1,
            vec![ColumnDef {
                name: "body".to_string(),
                data_type: DataType::Tsvector,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            }],
            vec![],
        )
    }

    #[test]
    fn test_build_scan_root_rejects_gin_scan_type() {
        let err = build_scan_root_from_scan_type(
            planner_test_schema(),
            ScanType::GinIndexScan {
                index_id: 7,
                index_name: "idx_body_gin".to_string(),
                column: "body".to_string(),
                pattern: Value::Tsquery("hello".to_string()),
                estimated_rows: 1,
            },
            None,
            None,
            &[],
        )
        .expect_err("GinIndexScan must not be built in operators planner");

        let msg = err.to_string();
        assert!(msg.contains("Unexpected ScanType::GinIndexScan"));
        assert!(msg.contains("table 't'"));
    }
}
