//! Logical plan representation.
//!
//! Immutable tree of [`LogicalNode`] variants. Each node carries a
//! [`PlanSchema`] (output column names + types) for downstream consumers.
//!
//! Design rules:
//! - Immutable trees with `Box<LogicalPlan>` children (no arena allocator).
//! - TypedExpr from the Analyzer is reused directly — no re-lowering.
//! - No cost or physical information in the logical plan.

use crate::model::DataType;
use crate::sql::analyzer::types::{
    AnalyzedProjection, JoinCondition, JoinType, SetOpKind, TypedExpr, TypedFunctionArg,
    TypedOrderByExpr,
};
use crate::sql::operators::WindowFunctionExpr;

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
    #[allow(dead_code)] // forward-compat: Phase 2+ logical plan variant
    pub fn empty() -> Self {
        Self {
            columns: Vec::new(),
        }
    }

    pub fn from_columns(columns: Vec<(String, DataType)>) -> Self {
        Self { columns }
    }

    #[allow(dead_code)] // forward-compat: Phase 2+ logical plan variant
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

    /// Semi-join: output left rows with ≥1 match in right.
    /// Output schema = left-side only (critical invariant).
    SemiJoin {
        left: Box<LogicalPlan>,
        right: Box<LogicalPlan>,
        condition: JoinCondition,
    },

    /// Anti-join: output left rows with 0 matches in right.
    /// Output schema = left-side only (critical invariant).
    AntiJoin {
        left: Box<LogicalPlan>,
        right: Box<LogicalPlan>,
        condition: JoinCondition,
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

    /// Wrap this plan in a SemiJoin node.
    /// Output schema = left-side only (this plan's schema).
    pub fn semi_join(self, right: LogicalPlan, condition: JoinCondition) -> Self {
        let schema = self.schema.clone(); // LEFT-ONLY
        Self {
            node: LogicalNode::SemiJoin {
                left: Box::new(self),
                right: Box::new(right),
                condition,
            },
            schema,
        }
    }

    /// Wrap this plan in an AntiJoin node.
    /// Output schema = left-side only (this plan's schema).
    pub fn anti_join(self, right: LogicalPlan, condition: JoinCondition) -> Self {
        let schema = self.schema.clone(); // LEFT-ONLY
        Self {
            node: LogicalNode::AntiJoin {
                left: Box::new(self),
                right: Box::new(right),
                condition,
            },
            schema,
        }
    }

    /// Apply `f` to every immediate child `LogicalPlan`, preserving all
    /// non-child fields.  Leaf nodes (Scan, Values, TableFunction, Empty) are
    /// returned unchanged.
    pub(crate) fn map_children(self, mut f: impl FnMut(LogicalPlan) -> LogicalPlan) -> LogicalPlan {
        let schema = self.schema;
        let node = match self.node {
            // ── Leaf nodes ──────────────────────────────────────
            node @ (LogicalNode::Scan { .. }
            | LogicalNode::Values { .. }
            | LogicalNode::TableFunction { .. }
            | LogicalNode::Empty) => node,

            // ── Unary operators ─────────────────────────────────
            LogicalNode::Filter { predicate, input } => LogicalNode::Filter {
                predicate,
                input: Box::new(f(*input)),
            },
            LogicalNode::Project { projections, input } => LogicalNode::Project {
                projections,
                input: Box::new(f(*input)),
            },
            LogicalNode::Aggregate {
                group_by,
                projections,
                input,
            } => LogicalNode::Aggregate {
                group_by,
                projections,
                input: Box::new(f(*input)),
            },
            LogicalNode::Sort { order_by, input } => LogicalNode::Sort {
                order_by,
                input: Box::new(f(*input)),
            },
            LogicalNode::Limit {
                limit,
                offset,
                input,
            } => LogicalNode::Limit {
                limit,
                offset,
                input: Box::new(f(*input)),
            },
            LogicalNode::Distinct { input } => LogicalNode::Distinct {
                input: Box::new(f(*input)),
            },
            LogicalNode::DistinctOn { on_exprs, input } => LogicalNode::DistinctOn {
                on_exprs,
                input: Box::new(f(*input)),
            },
            LogicalNode::Window {
                window_functions,
                input_col_count,
                input,
            } => LogicalNode::Window {
                window_functions,
                input_col_count,
                input: Box::new(f(*input)),
            },
            LogicalNode::Subquery { subplan, alias } => LogicalNode::Subquery {
                subplan: Box::new(f(*subplan)),
                alias,
            },

            // ── Binary operators ────────────────────────────────
            LogicalNode::Join {
                left,
                right,
                join_type,
                condition,
            } => LogicalNode::Join {
                left: Box::new(f(*left)),
                right: Box::new(f(*right)),
                join_type,
                condition,
            },
            LogicalNode::SetOperation {
                op,
                all,
                left,
                right,
            } => LogicalNode::SetOperation {
                op,
                all,
                left: Box::new(f(*left)),
                right: Box::new(f(*right)),
            },
            LogicalNode::SemiJoin {
                left,
                right,
                condition,
            } => LogicalNode::SemiJoin {
                left: Box::new(f(*left)),
                right: Box::new(f(*right)),
                condition,
            },
            LogicalNode::AntiJoin {
                left,
                right,
                condition,
            } => LogicalNode::AntiJoin {
                left: Box::new(f(*left)),
                right: Box::new(f(*right)),
                condition,
            },
        };
        LogicalPlan { node, schema }
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

#[cfg(test)]
mod tests {
    use super::{LogicalNode, LogicalPlan, PlanSchema};
    use crate::model::{DataType, Value};
    use crate::sql::analyzer::types::{
        JoinCondition, JoinType, SetOpKind, TypedExpr, TypedExprKind,
    };

    fn one_col_schema(name: &str) -> PlanSchema {
        PlanSchema::from_columns(vec![(name.to_string(), DataType::Int64)])
    }

    fn two_col_schema(left: &str, right: &str) -> PlanSchema {
        PlanSchema::from_columns(vec![
            (left.to_string(), DataType::Int64),
            (right.to_string(), DataType::Int64),
        ])
    }

    fn scan(name: &str) -> LogicalPlan {
        LogicalPlan::scan(
            name.to_string(),
            None,
            one_col_schema(&format!("{name}_id")),
        )
    }

    fn bool_const(v: bool) -> TypedExpr {
        TypedExpr::new(
            TypedExprKind::Constant(Value::Boolean(v)),
            DataType::Boolean,
        )
    }

    fn assert_schema_names(plan: &LogicalPlan, expected: &[&str]) {
        let actual: Vec<&str> = plan
            .schema
            .columns
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(actual, expected);
    }

    fn rename_scan_table(plan: LogicalPlan, new_name: &str) -> LogicalPlan {
        match plan {
            LogicalPlan {
                node: LogicalNode::Scan { alias, .. },
                schema,
            } => LogicalPlan {
                node: LogicalNode::Scan {
                    table_name: new_name.to_string(),
                    alias,
                },
                schema,
            },
            other => other,
        }
    }

    fn assert_map_invocations(plan: LogicalPlan, expected_calls: usize) {
        let mut calls = 0usize;
        let _ = plan.map_children(|child| {
            calls += 1;
            child
        });
        assert_eq!(calls, expected_calls);
    }

    #[test]
    fn map_children_leaf_nodes_are_identity() {
        let mut calls = 0usize;
        let plan = scan("t");
        let mapped = plan.clone().map_children(|child| {
            calls += 1;
            rename_scan_table(child, "should_not_run")
        });

        assert_eq!(calls, 0);
        match &mapped.node {
            LogicalNode::Scan { table_name, alias } => {
                assert_eq!(table_name, "t");
                assert!(alias.is_none());
            }
            other => panic!("expected Scan, got {other:?}"),
        }
        assert_schema_names(&mapped, &["t_id"]);
    }

    #[test]
    fn map_children_unary_preserves_fields_and_maps_single_child() {
        let plan = LogicalPlan {
            node: LogicalNode::Filter {
                predicate: bool_const(true),
                input: Box::new(scan("base")),
            },
            schema: one_col_schema("base_id"),
        };

        let mut calls = 0usize;
        let mapped = plan.map_children(|child| {
            calls += 1;
            rename_scan_table(child, "mapped_base")
        });

        assert_eq!(calls, 1);
        match &mapped.node {
            LogicalNode::Filter { predicate, input } => {
                match &predicate.kind {
                    TypedExprKind::Constant(Value::Boolean(v)) => assert!(v),
                    other => panic!("expected boolean constant predicate, got {other:?}"),
                }
                match &input.node {
                    LogicalNode::Scan { table_name, .. } => assert_eq!(table_name, "mapped_base"),
                    other => panic!("expected mapped scan input, got {other:?}"),
                }
            }
            other => panic!("expected Filter, got {other:?}"),
        }
        assert_schema_names(&mapped, &["base_id"]);
    }

    #[test]
    fn map_children_binary_maps_both_children_in_left_to_right_order() {
        let plan = LogicalPlan {
            node: LogicalNode::Join {
                left: Box::new(scan("left_src")),
                right: Box::new(scan("right_src")),
                join_type: JoinType::Inner,
                condition: JoinCondition::None,
            },
            schema: two_col_schema("left_id", "right_id"),
        };

        let mut visited = Vec::new();
        let mapped = plan.map_children(|child| match child {
            LogicalPlan {
                node: LogicalNode::Scan { table_name, alias },
                schema,
            } => {
                visited.push(table_name.clone());
                LogicalPlan {
                    node: LogicalNode::Scan {
                        table_name: format!("mapped_{table_name}"),
                        alias,
                    },
                    schema,
                }
            }
            other => other,
        });

        assert_eq!(
            visited,
            vec!["left_src".to_string(), "right_src".to_string()]
        );
        match &mapped.node {
            LogicalNode::Join {
                left,
                right,
                join_type,
                condition,
            } => {
                assert_eq!(join_type, &JoinType::Inner);
                assert!(matches!(condition, JoinCondition::None));
                match &left.node {
                    LogicalNode::Scan { table_name, .. } => {
                        assert_eq!(table_name, "mapped_left_src")
                    }
                    other => panic!("expected mapped left scan, got {other:?}"),
                }
                match &right.node {
                    LogicalNode::Scan { table_name, .. } => {
                        assert_eq!(table_name, "mapped_right_src")
                    }
                    other => panic!("expected mapped right scan, got {other:?}"),
                }
            }
            other => panic!("expected Join, got {other:?}"),
        }
        assert_schema_names(&mapped, &["left_id", "right_id"]);
    }

    #[test]
    fn map_children_subquery_maps_subplan_and_preserves_alias() {
        let plan = LogicalPlan {
            node: LogicalNode::Subquery {
                subplan: Box::new(scan("inner_sq")),
                alias: Some("sq_alias".to_string()),
            },
            schema: one_col_schema("sq_col"),
        };

        let mapped = plan.map_children(|child| rename_scan_table(child, "mapped_inner_sq"));
        match &mapped.node {
            LogicalNode::Subquery { subplan, alias } => {
                assert_eq!(alias.as_deref(), Some("sq_alias"));
                match &subplan.node {
                    LogicalNode::Scan { table_name, .. } => {
                        assert_eq!(table_name, "mapped_inner_sq")
                    }
                    other => panic!("expected mapped subplan scan, got {other:?}"),
                }
            }
            other => panic!("expected Subquery, got {other:?}"),
        }
        assert_schema_names(&mapped, &["sq_col"]);
    }

    #[test]
    fn map_children_invocation_count_matches_node_arity() {
        assert_map_invocations(scan("scan_leaf"), 0);
        assert_map_invocations(
            LogicalPlan {
                node: LogicalNode::Values { rows: vec![] },
                schema: one_col_schema("v"),
            },
            0,
        );
        assert_map_invocations(
            LogicalPlan {
                node: LogicalNode::TableFunction {
                    function_name: "generate_series".to_string(),
                    args: vec![],
                    alias: None,
                },
                schema: one_col_schema("tf"),
            },
            0,
        );
        assert_map_invocations(LogicalPlan::empty(one_col_schema("e")), 0);

        assert_map_invocations(
            LogicalPlan {
                node: LogicalNode::Project {
                    projections: vec![],
                    input: Box::new(scan("p")),
                },
                schema: one_col_schema("p"),
            },
            1,
        );
        assert_map_invocations(
            LogicalPlan {
                node: LogicalNode::Aggregate {
                    group_by: vec![],
                    projections: vec![],
                    input: Box::new(scan("a")),
                },
                schema: one_col_schema("a"),
            },
            1,
        );
        assert_map_invocations(
            LogicalPlan {
                node: LogicalNode::Sort {
                    order_by: vec![],
                    input: Box::new(scan("s")),
                },
                schema: one_col_schema("s"),
            },
            1,
        );
        assert_map_invocations(
            LogicalPlan {
                node: LogicalNode::Limit {
                    limit: None,
                    offset: None,
                    input: Box::new(scan("l")),
                },
                schema: one_col_schema("l"),
            },
            1,
        );
        assert_map_invocations(
            LogicalPlan {
                node: LogicalNode::Distinct {
                    input: Box::new(scan("d")),
                },
                schema: one_col_schema("d"),
            },
            1,
        );
        assert_map_invocations(
            LogicalPlan {
                node: LogicalNode::DistinctOn {
                    on_exprs: vec![],
                    input: Box::new(scan("do")),
                },
                schema: one_col_schema("do"),
            },
            1,
        );
        assert_map_invocations(
            LogicalPlan {
                node: LogicalNode::Window {
                    window_functions: vec![],
                    input_col_count: 1,
                    input: Box::new(scan("w")),
                },
                schema: one_col_schema("w"),
            },
            1,
        );
        assert_map_invocations(
            LogicalPlan {
                node: LogicalNode::Subquery {
                    subplan: Box::new(scan("sq")),
                    alias: Some("x".to_string()),
                },
                schema: one_col_schema("sq"),
            },
            1,
        );

        assert_map_invocations(
            LogicalPlan {
                node: LogicalNode::SetOperation {
                    op: SetOpKind::Union,
                    all: false,
                    left: Box::new(scan("set_l")),
                    right: Box::new(scan("set_r")),
                },
                schema: one_col_schema("set"),
            },
            2,
        );
        assert_map_invocations(
            LogicalPlan {
                node: LogicalNode::SemiJoin {
                    left: Box::new(scan("semi_l")),
                    right: Box::new(scan("semi_r")),
                    condition: JoinCondition::None,
                },
                schema: one_col_schema("semi"),
            },
            2,
        );
        assert_map_invocations(
            LogicalPlan {
                node: LogicalNode::AntiJoin {
                    left: Box::new(scan("anti_l")),
                    right: Box::new(scan("anti_r")),
                    condition: JoinCondition::None,
                },
                schema: one_col_schema("anti"),
            },
            2,
        );
    }
}
