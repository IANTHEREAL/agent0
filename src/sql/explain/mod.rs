//! EXPLAIN statement - PostgreSQL-compatible query plan output.
//!
//! This module provides:
//! - `PlanNode` / `PlanCost` — the display-oriented plan tree
//! - `physical_plan_to_plan_node` — converts optimizer output to `PlanNode`
//! - `format_plan_text` — renders a `PlanNode` tree as EXPLAIN text

mod format;
mod transform;

#[cfg(test)]
mod tests;

// Re-export public API
pub use format::format_plan_text;
pub(crate) use format::format_typed_expr;
pub use transform::physical_plan_to_plan_node;

const DEFAULT_ROW_WIDTH: usize = 40;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanAnnotations {
    pub task: Option<String>,
    pub output: Option<Vec<String>>,
    pub pushed_down: Vec<String>,
    pub storage_access: Option<String>,
    pub storage_limit: Option<usize>,
}

#[derive(Debug, Clone)]
pub enum PlanNode {
    SeqScan {
        table_name: String,
        alias: Option<String>,
        filter: Option<String>,
        annotations: PlanAnnotations,
        cost: PlanCost,
    },
    IndexScan {
        table_name: String,
        alias: Option<String>,
        index_name: String,
        index_cond: Option<String>,
        filter: Option<String>,
        annotations: PlanAnnotations,
        cost: PlanCost,
    },
    HnswScan {
        table_name: String,
        alias: Option<String>,
        index_name: String,
        distance_metric: String,
        k: usize,
        annotations: PlanAnnotations,
        cost: PlanCost,
    },
    NestedLoop {
        join_type: String,
        cost: PlanCost,
        children: Vec<PlanNode>,
    },
    HashJoin {
        join_type: String,
        hash_cond: Option<String>,
        cost: PlanCost,
        children: Vec<PlanNode>,
    },
    SemiJoin {
        anti: bool,
        hash_cond: Option<String>,
        cost: PlanCost,
        children: Vec<PlanNode>,
    },
    Sort {
        sort_key: Vec<String>,
        cost: PlanCost,
        child: Box<PlanNode>,
    },
    Limit {
        cost: PlanCost,
        child: Box<PlanNode>,
    },
    Aggregate {
        strategy: String,
        keys: Vec<String>,
        cost: PlanCost,
        child: Box<PlanNode>,
    },
    TableFunctionScan {
        function_name: String,
        alias: Option<String>,
        cost: PlanCost,
    },
    Filter {
        condition: String,
        cost: PlanCost,
        child: Box<PlanNode>,
    },
    Result {
        cost: PlanCost,
    },
}

#[derive(Debug, Clone)]
pub struct PlanCost {
    pub startup: f64,
    pub total: f64,
    pub rows: usize,
    pub width: usize,
}

impl Default for PlanCost {
    fn default() -> Self {
        Self {
            startup: 0.0,
            total: 0.0,
            rows: 1,
            width: DEFAULT_ROW_WIDTH,
        }
    }
}
