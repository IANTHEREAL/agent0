//! Cost-Based Optimizer (CBO) — Phase 2: LogicalPlan pipeline + selectivity.
//!
//! Translates `AnalyzedQuery → LogicalPlan → PhysicalPlan → BoxedOperator`.
//! Uses table statistics (when available via ANALYZE) for selectivity estimation
//! and cardinality estimates. Falls back to legacy heuristics when no stats exist.

pub mod build;
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
