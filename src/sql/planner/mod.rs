//! Query planner for cost-based optimization
//!
//! This module provides query planning and optimization capabilities:
//! - Cost-based index selection
//! - Predicate analysis
//! - Access path selection

mod cost_model;
mod index_selection;
mod predicate;
mod scan_type;
#[cfg(test)]
mod tests;

// Re-export public types and functions so external code continues to work
// with `crate::sql::planner::Foo` paths unchanged.

pub use self::index_selection::choose_btree_access_path_for_typed_filter;
pub(crate) use self::predicate::collect_typed_eq_predicates;

use crate::model::Value;

#[derive(Debug, Clone)]
#[allow(clippy::enum_variant_names)]
pub enum ScanType {
    FullTableScan,
    IndexScan {
        index_id: u64,
        index_name: String,
        values: Vec<Value>,
    },
    IndexRangeScan {
        index_id: u64,
        index_name: String,
        prefix_values: Vec<Value>,
    },
    IndexBoundedRangeScan {
        index_id: u64,
        index_name: String,
        prefix_values: Vec<Value>,
        range_start: Option<Value>,
        start_inclusive: bool,
        range_end: Option<Value>,
        end_inclusive: bool,
    },
    InListScan {
        index_id: u64,
        index_name: String,
        column_values: Vec<Vec<Value>>,
    },
}

#[derive(Debug, Clone)]
pub struct AccessPath {
    pub scan_type: ScanType,
    pub cost: f64,
}

/// Comparison operators for scalar predicates (col OP const).
#[derive(Debug, Clone, PartialEq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// A typed predicate extracted from a filter expression for index planning.
///
/// Each variant carries exactly the fields it needs — no sentinel values, no
/// dual-purpose fields that mean different things depending on `op`.
///
/// IS NULL / IS NOT NULL are not represented here because B-tree indexes cannot
/// be used to satisfy those predicates in the current planner.
#[derive(Debug, Clone)]
pub enum TypedPredicate {
    /// `column OP constant` where OP is a scalar comparison.
    Comparison {
        column: String,
        op: CmpOp,
        value: Value,
    },
    /// `column IN (v1, v2, ...)` — all list elements are constants.
    InList { column: String, values: Vec<Value> },
}
