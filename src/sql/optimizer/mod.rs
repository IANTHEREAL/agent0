//! Cost-Based Optimizer (CBO) — Phase 3: LogicalPlan pipeline + joins.
//!
//! Translates `AnalyzedQuery → LogicalPlan → PhysicalPlan → BoxedOperator`.
//! Uses table statistics (when available via ANALYZE) for selectivity estimation
//! and cardinality estimates. Falls back to legacy heuristics when no stats exist.

pub mod build;
pub mod eligibility;
pub mod logical_plan;
pub mod logical_planner;
pub mod physical_plan;
pub mod physical_planner;
pub mod selectivity;
pub mod statistics;

pub use build::BuildContext;
pub use logical_planner::LogicalPlanner;
pub use physical_planner::{PhysicalPlanner, PlanningContext};
pub use statistics::{ColumnStatistics, TableStatistics};

use crate::sql::analyzer::types::AnalyzedQuery;
use physical_plan::PhysicalPlan;

/// Single optimizer entrypoint: AnalyzedQuery → PhysicalPlan.
///
/// Execution (`execute_via_optimizer`) and EXPLAIN (`statement.rs`) both call
/// this function when `use_optimizer` is on and the query is eligible, ensuring
/// they produce identical plans — no drift.
pub fn optimize(analyzed: &AnalyzedQuery, planning_ctx: &PlanningContext) -> PhysicalPlan {
    // Step 1: AnalyzedQuery → LogicalPlan
    let logical = LogicalPlanner::build(analyzed);
    // Step 2: LogicalPlan → PhysicalPlan (cost-based)
    PhysicalPlanner::plan(&logical, planning_ctx)
}
