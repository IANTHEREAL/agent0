//! Cost-Based Optimizer (CBO) — Phase 1: LogicalPlan pipeline.
//!
//! Translates `AnalyzedQuery → LogicalPlan → PhysicalPlan → BoxedOperator`.
//! Phase 1 targets single-table SELECTs with no optimizer rules or statistics.

pub mod build;
pub mod logical_plan;
pub mod logical_planner;
pub mod physical_plan;
pub mod physical_planner;
pub mod statistics;

pub use build::BuildContext;
pub use logical_planner::LogicalPlanner;
pub use physical_planner::PhysicalPlanner;
pub use statistics::{ColumnStatistics, TableStatistics};
