//! Logical plan representation.
//!
//! Immutable tree of [`LogicalNode`] variants. Each node carries a
//! [`PlanSchema`] (output column names + types) for downstream consumers.
//!
//! Design rules:
//! - Immutable trees with `Box<LogicalPlan>` children (no arena allocator).
//! - TypedExpr from the Analyzer is reused directly — no re-lowering.
//! - No cost or physical information in the logical plan.

use crate::sql::analyzer::types::{
    AnalyzedProjection, JoinCondition, JoinType, SetOpKind, TypedExpr, TypedFunctionArg,
    TypedOrderByExpr,
};
use crate::sql::operators::WindowFunctionExpr;
use crate::types::DataType;

/// A logical plan tree node.
#[derive(Debug, Clone)]
pub struct LogicalPlan {
    /// The logical operation.
    pub node: LogicalNode,
    /// Output schema (column_name, data_type) for each output column.
    pub schema: PlanSchema,
}

/// Output schema of a plan node.
#[derive(Debug, Clone)]
pub struct PlanSchema {
    pub columns: Vec<(String, DataType)>,
}

impl PlanSchema {
    #[allow(dead_code)] // Phase 2+
    pub fn empty() -> Self {
        Self {
            columns: Vec::new(),
        }
    }

    pub fn from_columns(columns: Vec<(String, DataType)>) -> Self {
        Self { columns }
    }

    #[allow(dead_code)] // Phase 2+
    pub fn num_columns(&self) -> usize {
        self.columns.len()
    }
}

/// Logical plan node variants.
///
/// Each variant represents a relational algebra operation. The optimizer
/// transforms these nodes via rewrite rules (Phase 3).
#[derive(Debug, Clone)]
pub enum LogicalNode {
    // ── Leaf nodes ──────────────────────────────────────
    /// Scan a base table.
    Scan {
        table_name: String,
        alias: Option<String>,
    },

    /// Inline values (from VALUES clause).
    Values { rows: Vec<Vec<TypedExpr>> },

    /// Table-valued function (e.g. generate_series, unnest).
    TableFunction {
        function_name: String,
        args: Vec<TypedFunctionArg>,
        alias: Option<String>,
    },

    /// No-input node for queries without FROM (e.g. SELECT 1).
    Empty,

    // ── Unary operators ─────────────────────────────────
    /// Filter rows by a predicate.
    Filter {
        predicate: TypedExpr,
        input: Box<LogicalPlan>,
    },

    /// Project (compute output columns).
    Project {
        projections: Vec<AnalyzedProjection>,
        input: Box<LogicalPlan>,
    },

    /// Aggregate with optional grouping keys.
    Aggregate {
        group_by: Vec<TypedExpr>,
        /// The full projection list (may contain aggregate calls).
        projections: Vec<AnalyzedProjection>,
        input: Box<LogicalPlan>,
    },

    /// Sort by one or more expressions.
    Sort {
        order_by: Vec<TypedOrderByExpr>,
        input: Box<LogicalPlan>,
    },

    /// Limit + offset.
    Limit {
        limit: Option<TypedExpr>,
        offset: Option<TypedExpr>,
        input: Box<LogicalPlan>,
    },

    /// DISTINCT (deduplicate all columns).
    Distinct { input: Box<LogicalPlan> },

    /// DISTINCT ON (deduplicate on specified expressions).
    DistinctOn {
        on_exprs: Vec<TypedExpr>,
        input: Box<LogicalPlan>,
    },

    /// Window function evaluation.
    ///
    /// Carries window function definitions and the input column count
    /// needed to construct `WindowOperator` (which appends window output
    /// columns after the input columns).
    Window {
        window_functions: Vec<WindowFunctionExpr>,
        input_col_count: usize,
        input: Box<LogicalPlan>,
    },

    // ── Binary operators ────────────────────────────────
    /// Join two inputs.
    Join {
        left: Box<LogicalPlan>,
        right: Box<LogicalPlan>,
        join_type: JoinType,
        condition: JoinCondition,
    },

    /// Set operation (UNION / INTERSECT / EXCEPT).
    SetOperation {
        op: SetOpKind,
        all: bool,
        left: Box<LogicalPlan>,
        right: Box<LogicalPlan>,
    },

    // ── Correlated ──────────────────────────────────────
    /// Subquery in FROM (opaque — treated as a single plan node).
    Subquery {
        subplan: Box<LogicalPlan>,
        alias: Option<String>,
    },
}

impl LogicalPlan {
    /// Create a scan node.
    pub fn scan(table_name: String, alias: Option<String>, schema: PlanSchema) -> Self {
        Self {
            node: LogicalNode::Scan { table_name, alias },
            schema,
        }
    }

    /// Create an empty (no-input) node.
    pub fn empty(schema: PlanSchema) -> Self {
        Self {
            node: LogicalNode::Empty,
            schema,
        }
    }

    /// Wrap this plan in a Filter node.
    pub fn filter(self, predicate: TypedExpr) -> Self {
        let schema = self.schema.clone();
        Self {
            node: LogicalNode::Filter {
                predicate,
                input: Box::new(self),
            },
            schema,
        }
    }

    /// Wrap this plan in a Project node.
    pub fn project(self, projections: Vec<AnalyzedProjection>, output_schema: PlanSchema) -> Self {
        Self {
            node: LogicalNode::Project {
                projections,
                input: Box::new(self),
            },
            schema: output_schema,
        }
    }

    /// Wrap this plan in an Aggregate node.
    pub fn aggregate(
        self,
        group_by: Vec<TypedExpr>,
        projections: Vec<AnalyzedProjection>,
        output_schema: PlanSchema,
    ) -> Self {
        Self {
            node: LogicalNode::Aggregate {
                group_by,
                projections,
                input: Box::new(self),
            },
            schema: output_schema,
        }
    }

    /// Wrap this plan in a Sort node.
    pub fn sort(self, order_by: Vec<TypedOrderByExpr>) -> Self {
        let schema = self.schema.clone();
        Self {
            node: LogicalNode::Sort {
                order_by,
                input: Box::new(self),
            },
            schema,
        }
    }

    /// Wrap this plan in a Limit node.
    pub fn limit(self, limit: Option<TypedExpr>, offset: Option<TypedExpr>) -> Self {
        let schema = self.schema.clone();
        Self {
            node: LogicalNode::Limit {
                limit,
                offset,
                input: Box::new(self),
            },
            schema,
        }
    }

    /// Wrap this plan in a Distinct node.
    pub fn distinct(self) -> Self {
        let schema = self.schema.clone();
        Self {
            node: LogicalNode::Distinct {
                input: Box::new(self),
            },
            schema,
        }
    }

    /// Wrap this plan in a DistinctOn node.
    pub fn distinct_on(self, on_exprs: Vec<TypedExpr>) -> Self {
        let schema = self.schema.clone();
        Self {
            node: LogicalNode::DistinctOn {
                on_exprs,
                input: Box::new(self),
            },
            schema,
        }
    }

    /// Wrap this plan in a Window node.
    ///
    /// The output schema extends the input with one column per window function.
    pub fn window(
        self,
        window_functions: Vec<WindowFunctionExpr>,
        output_schema: PlanSchema,
    ) -> Self {
        let input_col_count = self.schema.columns.len();
        Self {
            node: LogicalNode::Window {
                window_functions,
                input_col_count,
                input: Box::new(self),
            },
            schema: output_schema,
        }
    }
}
