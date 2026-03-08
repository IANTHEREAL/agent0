//! Query planner for cost-based optimization
//!
//! This module provides query planning and optimization capabilities:
//! - Cost-based index selection
//! - Predicate analysis
//! - Access path selection

mod cost_model;
pub(crate) mod gin_predicate;
pub mod hnsw_predicate;
mod index_selection;
mod predicate;
mod scan_type;
#[cfg(test)]
mod tests;

// Re-export public types and functions so external code continues to work
// with `crate::sql::planner::Foo` paths unchanged.

pub use self::index_selection::choose_btree_access_path_for_typed_filter;
pub(crate) use self::predicate::collect_typed_eq_predicates;

use self::hnsw_predicate::HnswQueryVector;
use crate::model::Value;

/// Boolean expression over GIN token hashes.
///
/// Used by `ScanType::GinIndexScan` to describe which posting lists to scan
/// and how to combine them.  The contract is that the resulting candidate set
/// is always a **superset** of the true result (false positives allowed, false
/// negatives are a correctness bug).  A `recheck` pass must filter out false
/// positives after row retrieval.
///
/// Pure negation (`Not` at the root with no positive anchor) cannot be indexed
/// and must be rejected by the planner.
#[derive(Debug, Clone)]
pub enum GinQual {
    /// A single token hash to look up in the GIN posting list.
    Term { token_hash: u64 },
    /// All children must match (intersection of posting lists).
    And(Vec<GinQual>),
    /// Any child may match (union of posting lists).
    Or(Vec<GinQual>),
    /// Negation — used only as a child of `And` (e.g. `A & !B`).  
    /// Never appears at the root of a `GinQual` tree.
    Not(Box<GinQual>),
}

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
    /// GIN inverted-index scan (FTS `@@`, JSONB `@>`, ARRAY `@>` / `&&`).
    ///
    /// The planner converts the predicate into a `GinQual` boolean tree of
    /// token hashes.  The runtime operator scans posting lists, applies set
    /// operations (intersect / union / difference), fetches candidate rows,
    /// and optionally rechecks the original predicate.
    GinIndexScan {
        index_id: u64,
        index_name: String,
        /// Boolean expression of token hashes (superset filter — no false negatives).
        qual: GinQual,
        /// The original predicate expression, used for per-row recheck after
        /// fetching candidate rows.  GIN scans always recheck because hash
        /// collisions and lossy tokenisation can produce false positives.
        #[allow(dead_code)]
        recheck_expr: Box<crate::sql::analyzer::types::TypedExpr>,
    },
    /// HNSW approximate nearest-neighbor index scan.
    ///
    /// The planner selects this scan type when a query uses vector distance
    /// operators (e.g., `<->` for L2 distance) on an HNSW-indexed column.
    /// The runtime operator loads the in-memory HNSW graph and performs
    /// beam search to find the k nearest neighbors.
    HnswIndexScan {
        index_id: u64,
        index_name: String,
        /// The query vector to search for nearest neighbors.
        query_vector: HnswQueryVector,
        /// Maximum number of results (from LIMIT clause).
        k: usize,
        /// Distance metric for HNSW search and SQL distance projection.
        distance_metric: crate::sql::hnsw::HnswDistanceMetric,
        /// The distance expression for projecting distance values.
        #[allow(dead_code)]
        distance_expr: Option<Box<crate::sql::analyzer::types::TypedExpr>>,
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
