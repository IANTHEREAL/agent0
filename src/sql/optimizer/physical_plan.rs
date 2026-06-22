//! Physical plan representation.
//!
//! Maps logical operations to physical execution strategies. Each node
//! carries a [`PhysicalCost`] estimate for the optimizer to compare
//! alternative plans (Phase 2+).

use super::logical_plan::PlanSchema;
use crate::sql::analyzer::types::{
    AnalyzedProjection, JoinCondition, JoinType, SetOpKind, TypedExpr, TypedFunctionArg,
    TypedOrderByExpr,
};
use crate::sql::operators::WindowFunctionExpr;
use crate::sql::planner::ScanType;

/// A physical plan tree node.
#[derive(Debug, Clone)]
pub struct PhysicalPlan {
    /// The physical operation.
    pub node: PhysicalNode,
    /// Output schema.
    pub schema: PlanSchema,
    /// Estimated cost.
    pub cost: PhysicalCost,
}

/// Cost estimate for a physical plan node.
#[derive(Debug, Clone, Default)]
pub struct PhysicalCost {
    /// Startup cost (time before first row is produced).
    pub startup: f64,
    /// Total cost (time to produce all rows).
    pub total: f64,
    /// Estimated number of output rows.
    pub rows: usize,
}

/// Base scan kind for a DB9 coprocessor physical node.
#[derive(Debug, Clone)]
pub enum Db9CopScan {
    Seq,
    Index {
        scan_type: ScanType,
        desc: bool,
        require_row_fetch: bool,
    },
}

/// Ordered storage-side operations fused into a DB9 coprocessor node.
#[derive(Debug, Clone)]
pub enum Db9CopOp {
    Filter {
        predicate: TypedExpr,
    },
    Project {
        projections: Vec<AnalyzedProjection>,
    },
    Limit {
        limit: usize,
    },
}

/// Physical plan node variants.
///
/// Each variant maps to a specific operator implementation.
#[derive(Debug, Clone)]
pub enum PhysicalNode {
    // ── Scan operators ──────────────────────────────────
    /// Sequential (full) table scan.
    SeqScan {
        table_name: String,
        alias: Option<String>,
    },

    /// B-tree index scan (point lookup, range, bounded-range, or in-list).
    ///
    /// Carries the full [`ScanType`] from the legacy planner, which encodes
    /// the index identity, lookup values, and range bounds needed to
    /// construct the appropriate scan operator in the build phase.
    /// GIN scans are excluded — they remain on SeqScan until a future milestone.
    IndexScan {
        table_name: String,
        alias: Option<String>,
        scan_type: ScanType,
    },

    /// HNSW approximate nearest-neighbor scan.
    ///
    /// The operator loads the in-memory HNSW graph and runs beam search
    /// to find the k nearest neighbors.
    HnswScan {
        table_name: String,
        alias: Option<String>,
        scan_type: ScanType,
    },

    /// DB9 coprocessor pushdown node.
    ///
    /// This is a planner-visible, explain-visible physical node representing a
    /// single-table scan plus an ordered unary chain fused for storage-side
    /// execution. Runtime execution is added in a later PR; until then any
    /// attempt to build operators from this node should fail explicitly.
    Db9Cop {
        table_name: String,
        alias: Option<String>,
        scan: Db9CopScan,
        ops: Vec<Db9CopOp>,
        /// Display-only column count for EXPLAIN when folded Project is hidden.
        display_column_count: usize,
    },

    /// No-input operator (for SELECT without FROM).
    Empty,

    /// Inline values.
    #[allow(dead_code)] // forward-compat: Phase 2+ Values rows field
    Values { rows: Vec<Vec<TypedExpr>> },

    /// Table-valued function.
    #[allow(dead_code)] // forward-compat: Phase 2+ TableFunction fields
    TableFunction {
        function_name: String,
        args: Vec<TypedFunctionArg>,
        alias: Option<String>,
    },

    // ── Unary operators ─────────────────────────────────
    /// Filter rows.
    Filter {
        predicate: TypedExpr,
        input: Box<PhysicalPlan>,
    },

    /// Compute output columns.
    Project {
        projections: Vec<AnalyzedProjection>,
        input: Box<PhysicalPlan>,
    },

    /// Hash-based aggregate.
    HashAggregate {
        group_by: Vec<TypedExpr>,
        projections: Vec<AnalyzedProjection>,
        input: Box<PhysicalPlan>,
    },

    /// Sort operator.
    Sort {
        order_by: Vec<TypedOrderByExpr>,
        input: Box<PhysicalPlan>,
    },

    /// TopN sort (sort + limit combined).
    TopNSort {
        order_by: Vec<TypedOrderByExpr>,
        limit: usize,
        input: Box<PhysicalPlan>,
    },

    /// Limit + offset.
    Limit {
        limit: Option<TypedExpr>,
        offset: Option<TypedExpr>,
        input: Box<PhysicalPlan>,
    },

    /// Distinct (hash-based deduplication).
    Distinct { input: Box<PhysicalPlan> },

    /// DISTINCT ON.
    DistinctOn {
        on_exprs: Vec<TypedExpr>,
        input: Box<PhysicalPlan>,
    },

    /// Window function evaluation.
    Window {
        window_functions: Vec<WindowFunctionExpr>,
        input: Box<PhysicalPlan>,
    },

    // ── Binary operators ────────────────────────────────
    /// Nested-loop join.
    NestedLoopJoin {
        left: Box<PhysicalPlan>,
        right: Box<PhysicalPlan>,
        join_type: JoinType,
        condition: JoinCondition,
    },

    /// Hash join (equi-join only).
    HashJoin {
        left: Box<PhysicalPlan>,
        right: Box<PhysicalPlan>,
        join_type: JoinType,
        condition: JoinCondition,
        /// Which input is the build side (true = left).
        left_is_build: bool,
    },

    /// Set operation.
    SetOperation {
        op: SetOpKind,
        all: bool,
        left: Box<PhysicalPlan>,
        right: Box<PhysicalPlan>,
    },

    /// Hash-based semi/anti join.
    /// Build side = always right. Output schema = left-side only.
    HashSemiJoin {
        left: Box<PhysicalPlan>,
        right: Box<PhysicalPlan>,
        anti: bool,
        condition: JoinCondition,
    },

    // ── Correlated ──────────────────────────────────────
    /// Subquery (opaque subplan).
    Subquery { subplan: Box<PhysicalPlan> },
}
