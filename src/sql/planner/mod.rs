//! Query planner for cost-based optimization
//!
//! This module provides query planning and optimization capabilities:
//! - Cost-based index selection
//! - Predicate analysis
//! - Access path selection

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

#[derive(Debug, Clone)]
pub struct PredicateInfo {
    pub column: String,
    pub op: PredicateOp,
    pub value: Option<Value>,
    pub in_values: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PredicateOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    In,
    IsNull,
    IsNotNull,
}
