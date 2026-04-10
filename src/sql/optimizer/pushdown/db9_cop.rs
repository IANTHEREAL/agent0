use crate::model::{DataType, Value};
use crate::sql::analyzer::types::{
    AnalyzedProjection, BinaryOp, FunctionKind, IsTestKind, TypedExpr, TypedExprKind, UnaryOp,
};
use crate::sql::optimizer::extract_constant_usize;
use crate::sql::optimizer::logical_plan::PlanSchema;
use crate::sql::optimizer::physical_plan::PhysicalNode::{
    Db9Cop, Distinct, DistinctOn, Filter, HashAggregate, HashJoin, HashSemiJoin, HnswScan, Limit,
    NestedLoopJoin, Project, SeqScan, SetOperation, Sort, Subquery, TableFunction, TopNSort,
    Values, Window,
};
use crate::sql::optimizer::physical_plan::{
    Db9CopOp, Db9CopScan, PhysicalCost, PhysicalNode, PhysicalPlan,
};
use crate::sql::planner::ScanType;
use std::collections::HashSet;

#[cfg(test)]
pub(crate) fn apply_db9_cop_folding(plan: PhysicalPlan) -> PhysicalPlan {
    apply_db9_cop_folding_inner(plan, None)
}

pub(crate) fn apply_db9_cop_folding_for_base_tables(
    plan: PhysicalPlan,
    base_table_keys: &HashSet<String>,
) -> PhysicalPlan {
    apply_db9_cop_folding_inner(plan, Some(base_table_keys))
}

fn apply_db9_cop_folding_inner(
    plan: PhysicalPlan,
    base_table_keys: Option<&HashSet<String>>,
) -> PhysicalPlan {
    if let PhysicalNode::Limit {
        limit,
        offset,
        input,
    } = &plan.node
    {
        if let Some(folded) = try_fold_limit_preserving_global(
            limit,
            offset,
            input,
            &plan.schema,
            &plan.cost,
            base_table_keys,
        ) {
            return folded;
        }
    }

    if let Some(candidate) = try_extract_candidate(&plan, base_table_keys) {
        if db9_cop_output_schema_supported(&plan.schema) {
            return candidate.into_plan(plan.schema.clone(), plan.cost.clone());
        }
    }

    let PhysicalPlan { node, schema, cost } = plan;
    let node = match node {
        Filter { predicate, input } => Filter {
            predicate,
            input: Box::new(apply_db9_cop_folding_inner(*input, base_table_keys)),
        },
        Project { projections, input } => Project {
            projections,
            input: Box::new(apply_db9_cop_folding_inner(*input, base_table_keys)),
        },
        HashAggregate {
            group_by,
            projections,
            input,
        } => HashAggregate {
            group_by,
            projections,
            input: Box::new(apply_db9_cop_folding_inner(*input, base_table_keys)),
        },
        Sort { order_by, input } => Sort {
            order_by,
            input: Box::new(apply_db9_cop_folding_inner(*input, base_table_keys)),
        },
        TopNSort {
            order_by,
            limit,
            input,
        } => TopNSort {
            order_by,
            limit,
            input: Box::new(apply_db9_cop_folding_inner(*input, base_table_keys)),
        },
        Limit {
            limit,
            offset,
            input,
        } => Limit {
            limit,
            offset,
            input: Box::new(apply_db9_cop_folding_inner(*input, base_table_keys)),
        },
        Distinct { input } => Distinct {
            input: Box::new(apply_db9_cop_folding_inner(*input, base_table_keys)),
        },
        DistinctOn { on_exprs, input } => DistinctOn {
            on_exprs,
            input: Box::new(apply_db9_cop_folding_inner(*input, base_table_keys)),
        },
        Window {
            window_functions,
            input,
        } => Window {
            window_functions,
            input: Box::new(apply_db9_cop_folding_inner(*input, base_table_keys)),
        },
        NestedLoopJoin {
            left,
            right,
            join_type,
            condition,
        } => NestedLoopJoin {
            left: Box::new(apply_db9_cop_folding_inner(*left, base_table_keys)),
            right: Box::new(apply_db9_cop_folding_inner(*right, base_table_keys)),
            join_type,
            condition,
        },
        HashJoin {
            left,
            right,
            join_type,
            condition,
            left_is_build,
        } => HashJoin {
            left: Box::new(apply_db9_cop_folding_inner(*left, base_table_keys)),
            right: Box::new(apply_db9_cop_folding_inner(*right, base_table_keys)),
            join_type,
            condition,
            left_is_build,
        },
        SetOperation {
            op,
            all,
            left,
            right,
        } => SetOperation {
            op,
            all,
            left: Box::new(apply_db9_cop_folding_inner(*left, base_table_keys)),
            right: Box::new(apply_db9_cop_folding_inner(*right, base_table_keys)),
        },
        HashSemiJoin {
            left,
            right,
            anti,
            condition,
        } => HashSemiJoin {
            left: Box::new(apply_db9_cop_folding_inner(*left, base_table_keys)),
            right: Box::new(apply_db9_cop_folding_inner(*right, base_table_keys)),
            anti,
            condition,
        },
        Subquery { subplan } => Subquery {
            subplan: Box::new(apply_db9_cop_folding_inner(*subplan, base_table_keys)),
        },
        other => other,
    };

    PhysicalPlan { node, schema, cost }
}

fn try_fold_limit_preserving_global(
    limit: &Option<crate::sql::analyzer::types::TypedExpr>,
    offset: &Option<crate::sql::analyzer::types::TypedExpr>,
    input: &PhysicalPlan,
    schema: &PlanSchema,
    cost: &PhysicalCost,
    base_table_keys: Option<&HashSet<String>>,
) -> Option<PhysicalPlan> {
    if !db9_cop_output_schema_supported(schema) {
        return None;
    }

    let pushed_limit = extract_pushdown_limit(limit, offset)?;
    let mut candidate = try_extract_candidate(input, base_table_keys)?;
    if !candidate.can_append_limit() {
        return None;
    }
    candidate.ops.push(Db9CopOp::Limit {
        limit: pushed_limit,
    });

    Some(PhysicalPlan {
        node: PhysicalNode::Limit {
            limit: limit.clone(),
            offset: offset.clone(),
            input: Box::new(candidate.into_plan(schema.clone(), input.cost.clone())),
        },
        schema: schema.clone(),
        cost: cost.clone(),
    })
}

#[derive(Debug, Clone)]
struct Db9CopCandidate {
    table_name: String,
    alias: Option<String>,
    scan: Db9CopScan,
    ops: Vec<Db9CopOp>,
    display_column_count: usize,
}

impl Db9CopCandidate {
    fn into_plan(
        self,
        schema: crate::sql::optimizer::logical_plan::PlanSchema,
        cost: PhysicalCost,
    ) -> PhysicalPlan {
        PhysicalPlan {
            node: Db9Cop {
                table_name: self.table_name,
                alias: self.alias,
                scan: self.scan,
                ops: self.ops,
                display_column_count: self.display_column_count,
            },
            schema,
            cost,
        }
    }

    fn can_append_filter(&self) -> bool {
        !self.ops.iter().any(|op| {
            matches!(
                op,
                Db9CopOp::Filter { .. } | Db9CopOp::Project { .. } | Db9CopOp::Limit { .. }
            )
        })
    }

    fn can_append_project(&self) -> bool {
        !self
            .ops
            .iter()
            .any(|op| matches!(op, Db9CopOp::Project { .. } | Db9CopOp::Limit { .. }))
    }

    fn can_append_limit(&self) -> bool {
        !self
            .ops
            .iter()
            .any(|op| matches!(op, Db9CopOp::Limit { .. }))
    }
}

fn try_extract_candidate(
    plan: &PhysicalPlan,
    base_table_keys: Option<&HashSet<String>>,
) -> Option<Db9CopCandidate> {
    match &plan.node {
        SeqScan { table_name, alias }
            if eligible_db9_cop_relation(table_name, alias.as_deref(), base_table_keys) =>
        {
            Some(Db9CopCandidate {
                table_name: table_name.clone(),
                alias: alias.clone(),
                scan: Db9CopScan::Seq,
                ops: Vec::new(),
                display_column_count: plan.schema.columns.len(),
            })
        }
        SeqScan { .. } => None,
        PhysicalNode::IndexScan {
            table_name,
            alias,
            scan_type,
        } if eligible_db9_cop_index_scan(scan_type)
            && eligible_db9_cop_relation(table_name, alias.as_deref(), base_table_keys) =>
        {
            Some(Db9CopCandidate {
                table_name: table_name.clone(),
                alias: alias.clone(),
                scan: Db9CopScan::Index {
                    scan_type: scan_type.clone(),
                },
                ops: Vec::new(),
                display_column_count: plan.schema.columns.len(),
            })
        }
        PhysicalNode::IndexScan { .. } => None,
        Filter { predicate, input } => {
            let mut candidate = try_extract_candidate(input, base_table_keys)?;
            if filter_is_redundant_for_scan(&candidate.scan, predicate) {
                return Some(candidate);
            }
            if !db9_cop_expr_supported(predicate) {
                return None;
            }
            if !candidate.can_append_filter() {
                return None;
            }
            candidate.ops.push(Db9CopOp::Filter {
                predicate: predicate.clone(),
            });
            Some(candidate)
        }
        Project { projections, input } => {
            let mut candidate = try_extract_candidate(input, base_table_keys)?;
            if !db9_cop_projections_supported(projections) {
                return None;
            }
            if !candidate.can_append_project() {
                return None;
            }
            candidate.ops.push(Db9CopOp::Project {
                projections: projections.clone(),
            });
            Some(candidate)
        }
        Limit {
            limit,
            offset,
            input,
        } => {
            let pushed_limit = extract_pushdown_limit(limit, offset)?;
            let mut candidate = try_extract_candidate(input, base_table_keys)?;
            if !candidate.can_append_limit() {
                return None;
            }
            candidate.ops.push(Db9CopOp::Limit {
                limit: pushed_limit,
            });
            Some(candidate)
        }
        Db9Cop { .. }
        | HnswScan { .. }
        | PhysicalNode::Empty
        | Values { .. }
        | TableFunction { .. }
        | HashAggregate { .. }
        | Sort { .. }
        | TopNSort { .. }
        | Distinct { .. }
        | DistinctOn { .. }
        | Window { .. }
        | NestedLoopJoin { .. }
        | HashJoin { .. }
        | SetOperation { .. }
        | HashSemiJoin { .. }
        | Subquery { .. } => None,
    }
}

fn eligible_db9_cop_index_scan(scan_type: &ScanType) -> bool {
    matches!(
        scan_type,
        ScanType::IndexScan { .. }
            | ScanType::IndexRangeScan { .. }
            | ScanType::IndexBoundedRangeScan { .. }
            | ScanType::InListScan { .. }
    )
}

fn eligible_db9_cop_relation(
    table_name: &str,
    alias: Option<&str>,
    base_table_keys: Option<&HashSet<String>>,
) -> bool {
    base_table_keys
        .is_none_or(|keys| keys.contains(&crate::sql::optimizer::schema_map_key(table_name, alias)))
}

fn extract_pushdown_limit(
    limit: &Option<crate::sql::analyzer::types::TypedExpr>,
    offset: &Option<crate::sql::analyzer::types::TypedExpr>,
) -> Option<usize> {
    let offset = offset
        .as_ref()
        .and_then(extract_constant_usize)
        .unwrap_or(0);
    if offset != 0 {
        return None;
    }
    limit.as_ref().and_then(extract_constant_usize)
}

fn db9_cop_projections_supported(projections: &[AnalyzedProjection]) -> bool {
    projections
        .iter()
        .all(|projection| db9_cop_expr_supported(&projection.expr))
}

fn db9_cop_output_schema_supported(schema: &PlanSchema) -> bool {
    schema
        .columns
        .iter()
        .all(|(_, data_type)| db9_cop_column_type_supported(data_type))
}

fn db9_cop_column_type_supported(data_type: &DataType) -> bool {
    matches!(
        data_type,
        DataType::Boolean
            | DataType::Int32
            | DataType::Int64
            | DataType::Float64
            | DataType::Text
            | DataType::Bytes
            | DataType::Timestamp
            | DataType::TimestampTz
            | DataType::Name
            | DataType::Varchar(_)
    )
}

fn db9_cop_expr_supported(expr: &TypedExpr) -> bool {
    match &expr.kind {
        TypedExprKind::Constant(value) => db9_cop_constant_supported(value),
        TypedExprKind::ColumnRef { scope_depth, .. } => {
            *scope_depth == 0 && db9_cop_column_type_supported(&expr.data_type)
        }
        TypedExprKind::BinaryOp { left, op, right } => {
            db9_cop_binary_op_supported(op)
                && db9_cop_expr_supported(left)
                && db9_cop_expr_supported(right)
        }
        TypedExprKind::UnaryOp { op, operand } => {
            db9_cop_unary_op_supported(op) && db9_cop_expr_supported(operand)
        }
        TypedExprKind::IsTest {
            expr,
            test: IsTestKind::Null,
            ..
        } => db9_cop_expr_supported(expr),
        TypedExprKind::FunctionCall {
            func,
            args,
            order_by,
            filter,
        } => {
            filter.is_none()
                && order_by.is_empty()
                && matches!(func.kind, FunctionKind::Builtin)
                && db9_cop_builtin_function_supported(&func.name, args.len())
                && args.iter().all(db9_cop_expr_supported)
        }
        TypedExprKind::Coalesce(exprs) => {
            !exprs.is_empty() && exprs.iter().all(db9_cop_expr_supported)
        }
        TypedExprKind::NullIf(left, right) => {
            db9_cop_expr_supported(left) && db9_cop_expr_supported(right)
        }
        TypedExprKind::Collate { expr, .. } => db9_cop_expr_supported(expr),
        _ => false,
    }
}

fn db9_cop_constant_supported(value: &Value) -> bool {
    matches!(
        value,
        Value::Null
            | Value::Boolean(_)
            | Value::Int32(_)
            | Value::Int64(_)
            | Value::Float64(_)
            | Value::Text(_)
            | Value::Bytes(_)
            | Value::Timestamp(_)
    )
}

fn db9_cop_binary_op_supported(op: &BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::Eq
            | BinaryOp::NotEq
            | BinaryOp::Lt
            | BinaryOp::LtEq
            | BinaryOp::Gt
            | BinaryOp::GtEq
            | BinaryOp::And
            | BinaryOp::Or
    )
}

fn db9_cop_unary_op_supported(op: &UnaryOp) -> bool {
    matches!(op, UnaryOp::Not | UnaryOp::Minus | UnaryOp::Plus)
}

fn db9_cop_builtin_function_supported(name: &str, arg_count: usize) -> bool {
    match name.to_ascii_lowercase().as_str() {
        "lower" | "upper" | "length" | "char_length" | "character_length" | "abs" => arg_count == 1,
        "coalesce" => arg_count >= 1,
        "nullif" => arg_count == 2,
        _ => false,
    }
}

fn filter_is_redundant_for_scan(scan: &Db9CopScan, predicate: &TypedExpr) -> bool {
    let Db9CopScan::Index { scan_type } = scan else {
        return false;
    };

    match scan_type {
        ScanType::IndexScan {
            lookup_column,
            values,
            ..
        } => filter_matches_single_column_exact_lookup(predicate, lookup_column.as_deref(), values),
        ScanType::InListScan {
            lookup_column,
            column_values,
            ..
        } => filter_matches_single_column_in_list_lookup(
            predicate,
            lookup_column.as_deref(),
            column_values,
        ),
        _ => false,
    }
}

fn filter_matches_single_column_exact_lookup(
    predicate: &TypedExpr,
    lookup_column: Option<&str>,
    values: &[Value],
) -> bool {
    let Some(lookup_column) = lookup_column else {
        return false;
    };
    if values.len() != 1 {
        return false;
    }

    let Some((column_name, value)) = single_column_equality_constant(predicate) else {
        return false;
    };

    column_name.eq_ignore_ascii_case(lookup_column) && value == values[0]
}

fn filter_matches_single_column_in_list_lookup(
    predicate: &TypedExpr,
    lookup_column: Option<&str>,
    column_values: &[Vec<Value>],
) -> bool {
    let Some(lookup_column) = lookup_column else {
        return false;
    };
    let Some((column_name, constants)) = normalized_single_column_in_list_constants(predicate)
    else {
        return false;
    };
    if !column_name.eq_ignore_ascii_case(lookup_column) {
        return false;
    }

    let expected = column_values
        .iter()
        .map(|values| match values.as_slice() {
            [value] => Some(value.clone()),
            _ => None,
        })
        .collect::<Option<Vec<_>>>();

    expected
        .as_ref()
        .is_some_and(|expected| expected == &constants)
}

fn single_column_equality_constant(predicate: &TypedExpr) -> Option<(String, Value)> {
    let TypedExprKind::BinaryOp { left, op, right } = &predicate.kind else {
        return None;
    };
    if *op != BinaryOp::Eq {
        return None;
    }

    match (&left.kind, &right.kind) {
        (
            TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_name,
                ..
            },
            TypedExprKind::Constant(value),
        )
        | (
            TypedExprKind::Constant(value),
            TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_name,
                ..
            },
        ) => Some((column_name.clone(), value.clone())),
        _ => None,
    }
}

fn normalized_single_column_in_list_constants(
    predicate: &TypedExpr,
) -> Option<(String, Vec<Value>)> {
    let TypedExprKind::InList {
        expr,
        list,
        negated: false,
    } = &predicate.kind
    else {
        return None;
    };

    let TypedExprKind::ColumnRef {
        scope_depth: 0,
        column_name,
        ..
    } = &expr.kind
    else {
        return None;
    };

    let mut values = Vec::with_capacity(list.len());
    for item in list {
        let TypedExprKind::Constant(value) = &item.kind else {
            return None;
        };
        if matches!(value, Value::Null) {
            continue;
        }
        if !values.contains(value) {
            values.push(value.clone());
        }
    }

    Some((column_name.clone(), values))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DataType, Value};
    use crate::sql::analyzer::types::{ResolvedFunction, TypedExprKind};
    use crate::sql::optimizer::logical_plan::PlanSchema;
    use crate::sql::types::CastContext;

    fn builtin(name: &str, return_type: DataType) -> ResolvedFunction {
        ResolvedFunction {
            name: name.to_owned(),
            kind: FunctionKind::Builtin,
            return_type,
        }
    }

    #[test]
    fn index_scan_folds_to_db9_cop() {
        let plan = PhysicalPlan {
            node: PhysicalNode::IndexScan {
                table_name: "users".to_owned(),
                alias: None,
                scan_type: ScanType::IndexScan {
                    index_id: 7,
                    index_name: "users_email_idx".to_owned(),
                    lookup_column: Some("email".to_owned()),
                    values: vec![Value::Text("a@example.com".to_owned())],
                },
            },
            schema: PlanSchema::from_columns(vec![("id".to_owned(), DataType::Int64)]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        match folded.node {
            PhysicalNode::Db9Cop {
                scan: Db9CopScan::Index { scan_type },
                ..
            } => assert!(matches!(scan_type, ScanType::IndexScan { .. })),
            other => panic!("expected Db9Cop Index scan, got {other:?}"),
        }
    }

    #[test]
    fn in_list_scan_folds_to_db9_cop() {
        let plan = PhysicalPlan {
            node: PhysicalNode::IndexScan {
                table_name: "users".to_owned(),
                alias: Some("u".to_owned()),
                scan_type: ScanType::InListScan {
                    index_id: 8,
                    index_name: "users_email_idx".to_owned(),
                    lookup_column: Some("email".to_owned()),
                    column_values: vec![
                        vec![Value::Text("a@example.com".to_owned())],
                        vec![Value::Text("b@example.com".to_owned())],
                    ],
                },
            },
            schema: PlanSchema::from_columns(vec![("id".to_owned(), DataType::Int64)]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        match folded.node {
            PhysicalNode::Db9Cop {
                scan: Db9CopScan::Index { scan_type },
                ..
            } => assert!(matches!(scan_type, ScanType::InListScan { .. })),
            other => panic!("expected Db9Cop InList scan, got {other:?}"),
        }
    }

    #[test]
    fn index_range_scan_folds_to_db9_cop() {
        let plan = PhysicalPlan {
            node: PhysicalNode::IndexScan {
                table_name: "users".to_owned(),
                alias: None,
                scan_type: ScanType::IndexRangeScan {
                    index_id: 9,
                    index_name: "users_status_created_at_idx".to_owned(),
                    prefix_values: vec![Value::Text("active".to_owned())],
                },
            },
            schema: PlanSchema::from_columns(vec![("id".to_owned(), DataType::Int64)]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        match folded.node {
            PhysicalNode::Db9Cop {
                scan: Db9CopScan::Index { scan_type },
                ..
            } => assert!(matches!(scan_type, ScanType::IndexRangeScan { .. })),
            other => panic!("expected Db9Cop IndexRange scan, got {other:?}"),
        }
    }

    #[test]
    fn bounded_index_range_folds_to_db9_cop() {
        let plan = PhysicalPlan {
            node: PhysicalNode::IndexScan {
                table_name: "users".to_owned(),
                alias: None,
                scan_type: ScanType::IndexBoundedRangeScan {
                    index_id: 9,
                    index_name: "users_email_idx".to_owned(),
                    prefix_values: vec![],
                    range_start: Some(Value::Text("a@example.com".to_owned())),
                    start_inclusive: true,
                    range_end: Some(Value::Text("c@example.com".to_owned())),
                    end_inclusive: false,
                },
            },
            schema: PlanSchema::from_columns(vec![("id".to_owned(), DataType::Int64)]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        match folded.node {
            PhysicalNode::Db9Cop {
                scan: Db9CopScan::Index { scan_type },
                ..
            } => assert!(matches!(scan_type, ScanType::IndexBoundedRangeScan { .. })),
            other => panic!("expected Db9Cop bounded IndexRange scan, got {other:?}"),
        }
    }

    #[test]
    fn redundant_in_list_filter_is_not_pushed_into_db9_cop() {
        let predicate = TypedExpr::new(
            TypedExprKind::InList {
                expr: Box::new(TypedExpr::new(
                    TypedExprKind::ColumnRef {
                        scope_depth: 0,
                        column_index: 1,
                        column_name: "email".to_owned(),
                    },
                    DataType::Text,
                )),
                list: vec![
                    TypedExpr::new(
                        TypedExprKind::Constant(Value::Text("a@example.com".to_owned())),
                        DataType::Text,
                    ),
                    TypedExpr::new(
                        TypedExprKind::Constant(Value::Text("a@example.com".to_owned())),
                        DataType::Text,
                    ),
                    TypedExpr::new(TypedExprKind::Constant(Value::Null), DataType::Text),
                    TypedExpr::new(
                        TypedExprKind::Constant(Value::Text("b@example.com".to_owned())),
                        DataType::Text,
                    ),
                ],
                negated: false,
            },
            DataType::Boolean,
        );
        let plan = PhysicalPlan {
            node: PhysicalNode::Filter {
                predicate,
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::IndexScan {
                        table_name: "users".to_owned(),
                        alias: None,
                        scan_type: ScanType::InListScan {
                            index_id: 8,
                            index_name: "users_email_idx".to_owned(),
                            lookup_column: Some("email".to_owned()),
                            column_values: vec![
                                vec![Value::Text("a@example.com".to_owned())],
                                vec![Value::Text("b@example.com".to_owned())],
                            ],
                        },
                    },
                    schema: PlanSchema::from_columns(vec![("id".to_owned(), DataType::Int64)]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("id".to_owned(), DataType::Int64)]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        match folded.node {
            PhysicalNode::Db9Cop { ops, .. } => assert!(ops.is_empty()),
            other => panic!("expected Db9Cop, got {other:?}"),
        }
    }

    #[test]
    fn residual_filter_above_in_list_scan_still_pushes() {
        let predicate = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(TypedExpr::new(
                    TypedExprKind::ColumnRef {
                        scope_depth: 0,
                        column_index: 2,
                        column_name: "active".to_owned(),
                    },
                    DataType::Boolean,
                )),
                op: BinaryOp::Eq,
                right: Box::new(TypedExpr::new(
                    TypedExprKind::Constant(Value::Boolean(true)),
                    DataType::Boolean,
                )),
            },
            DataType::Boolean,
        );
        let plan = PhysicalPlan {
            node: PhysicalNode::Filter {
                predicate,
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::IndexScan {
                        table_name: "users".to_owned(),
                        alias: None,
                        scan_type: ScanType::InListScan {
                            index_id: 8,
                            index_name: "users_email_idx".to_owned(),
                            lookup_column: Some("email".to_owned()),
                            column_values: vec![vec![Value::Text("a@example.com".to_owned())]],
                        },
                    },
                    schema: PlanSchema::from_columns(vec![("id".to_owned(), DataType::Int64)]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("id".to_owned(), DataType::Int64)]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        match folded.node {
            PhysicalNode::Db9Cop { ops, .. } => {
                assert_eq!(ops.len(), 1);
                assert!(matches!(ops[0], Db9CopOp::Filter { .. }));
            }
            other => panic!("expected Db9Cop, got {other:?}"),
        }
    }

    #[test]
    fn wrong_column_filter_above_in_list_scan_is_not_elided() {
        let predicate = TypedExpr::new(
            TypedExprKind::InList {
                expr: Box::new(TypedExpr::new(
                    TypedExprKind::ColumnRef {
                        scope_depth: 0,
                        column_index: 2,
                        column_name: "status".to_owned(),
                    },
                    DataType::Text,
                )),
                list: vec![
                    TypedExpr::new(
                        TypedExprKind::Constant(Value::Text("a@example.com".to_owned())),
                        DataType::Text,
                    ),
                    TypedExpr::new(
                        TypedExprKind::Constant(Value::Text("b@example.com".to_owned())),
                        DataType::Text,
                    ),
                ],
                negated: false,
            },
            DataType::Boolean,
        );
        let plan = PhysicalPlan {
            node: PhysicalNode::Filter {
                predicate,
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::IndexScan {
                        table_name: "users".to_owned(),
                        alias: None,
                        scan_type: ScanType::InListScan {
                            index_id: 8,
                            index_name: "users_email_idx".to_owned(),
                            lookup_column: Some("email".to_owned()),
                            column_values: vec![
                                vec![Value::Text("a@example.com".to_owned())],
                                vec![Value::Text("b@example.com".to_owned())],
                            ],
                        },
                    },
                    schema: PlanSchema::from_columns(vec![("id".to_owned(), DataType::Int64)]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("id".to_owned(), DataType::Int64)]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        assert!(matches!(folded.node, PhysicalNode::Filter { .. }));
    }

    #[test]
    fn supported_function_projection_folds_to_db9_cop() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![crate::sql::analyzer::types::AnalyzedProjection {
                    expr: TypedExpr::new(
                        TypedExprKind::FunctionCall {
                            func: builtin("lower", DataType::Text),
                            args: vec![TypedExpr::new(
                                TypedExprKind::ColumnRef {
                                    scope_depth: 0,
                                    column_index: 1,
                                    column_name: "v".to_owned(),
                                },
                                DataType::Text,
                            )],
                            order_by: vec![],
                            filter: None,
                        },
                        DataType::Text,
                    ),
                    output_name: "lower".to_owned(),
                }],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::IndexScan {
                        table_name: "users".to_owned(),
                        alias: None,
                        scan_type: ScanType::IndexScan {
                            index_id: 7,
                            index_name: "users_email_idx".to_owned(),
                            lookup_column: Some("email".to_owned()),
                            values: vec![Value::Text("a@example.com".to_owned())],
                        },
                    },
                    schema: PlanSchema::from_columns(vec![("v".to_owned(), DataType::Text)]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("lower".to_owned(), DataType::Text)]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        match folded.node {
            PhysicalNode::Db9Cop { ops, .. } => {
                assert_eq!(ops.len(), 1);
                assert!(matches!(ops[0], Db9CopOp::Project { .. }));
            }
            other => panic!("expected Db9Cop, got {other:?}"),
        }
    }

    #[test]
    fn uppercase_builtin_function_projection_folds_to_db9_cop() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![crate::sql::analyzer::types::AnalyzedProjection {
                    expr: TypedExpr::new(
                        TypedExprKind::FunctionCall {
                            func: builtin("LOWER", DataType::Text),
                            args: vec![TypedExpr::new(
                                TypedExprKind::ColumnRef {
                                    scope_depth: 0,
                                    column_index: 1,
                                    column_name: "v".to_owned(),
                                },
                                DataType::Text,
                            )],
                            order_by: vec![],
                            filter: None,
                        },
                        DataType::Text,
                    ),
                    output_name: "lower".to_owned(),
                }],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::IndexScan {
                        table_name: "users".to_owned(),
                        alias: None,
                        scan_type: ScanType::IndexScan {
                            index_id: 7,
                            index_name: "users_email_idx".to_owned(),
                            lookup_column: Some("email".to_owned()),
                            values: vec![Value::Text("a@example.com".to_owned())],
                        },
                    },
                    schema: PlanSchema::from_columns(vec![("v".to_owned(), DataType::Text)]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("lower".to_owned(), DataType::Text)]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        match folded.node {
            PhysicalNode::Db9Cop { ops, .. } => {
                assert_eq!(ops.len(), 1);
                assert!(matches!(ops[0], Db9CopOp::Project { .. }));
            }
            other => panic!("expected Db9Cop, got {other:?}"),
        }
    }

    #[test]
    fn non_base_seq_scan_does_not_fold_when_base_table_set_is_provided() {
        let plan = PhysicalPlan {
            node: PhysicalNode::SeqScan {
                table_name: "information_schema.tables".to_owned(),
                alias: None,
            },
            schema: PlanSchema::from_columns(vec![
                ("table_schema".to_owned(), DataType::Text),
                ("table_name".to_owned(), DataType::Text),
            ]),
            cost: PhysicalCost::default(),
        };
        let mut base_table_keys = HashSet::new();
        base_table_keys.insert("public.users".to_owned());

        let folded = apply_db9_cop_folding_for_base_tables(plan, &base_table_keys);
        assert!(matches!(folded.node, PhysicalNode::SeqScan { .. }));
    }

    #[test]
    fn unsupported_cast_projection_stays_local() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![crate::sql::analyzer::types::AnalyzedProjection {
                    expr: TypedExpr::new(
                        TypedExprKind::Cast {
                            expr: Box::new(TypedExpr::new(
                                TypedExprKind::ColumnRef {
                                    scope_depth: 0,
                                    column_index: 1,
                                    column_name: "v".to_owned(),
                                },
                                DataType::Text,
                            )),
                            target_type: DataType::Text,
                            cast_context: CastContext::Explicit,
                        },
                        DataType::Text,
                    ),
                    output_name: "?column?".to_owned(),
                }],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::IndexScan {
                        table_name: "users".to_owned(),
                        alias: None,
                        scan_type: ScanType::IndexScan {
                            index_id: 7,
                            index_name: "users_email_idx".to_owned(),
                            lookup_column: Some("email".to_owned()),
                            values: vec![Value::Text("a@example.com".to_owned())],
                        },
                    },
                    schema: PlanSchema::from_columns(vec![("v".to_owned(), DataType::Text)]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("?column?".to_owned(), DataType::Text)]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        match folded.node {
            PhysicalNode::Project { input, .. } => {
                assert!(matches!(input.node, PhysicalNode::Db9Cop { .. }));
            }
            other => panic!("expected local Project over Db9Cop child, got {other:?}"),
        }
    }

    #[test]
    fn timestamp_output_scan_folds_to_db9_cop() {
        let plan = PhysicalPlan {
            node: PhysicalNode::SeqScan {
                table_name: "events".to_owned(),
                alias: None,
            },
            schema: PlanSchema::from_columns(vec![
                ("id".to_owned(), DataType::Int64),
                ("created_at".to_owned(), DataType::Timestamp),
            ]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        assert!(matches!(folded.node, PhysicalNode::Db9Cop { .. }));
    }

    #[test]
    fn pushed_limit_keeps_global_limit_node() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Limit {
                limit: Some(TypedExpr::new(
                    TypedExprKind::Constant(Value::Int32(1)),
                    DataType::Int32,
                )),
                offset: None,
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::IndexScan {
                        table_name: "users".to_owned(),
                        alias: None,
                        scan_type: ScanType::IndexBoundedRangeScan {
                            index_id: 9,
                            index_name: "users_email_idx".to_owned(),
                            prefix_values: vec![],
                            range_start: Some(Value::Text("a@example.com".to_owned())),
                            start_inclusive: true,
                            range_end: None,
                            end_inclusive: false,
                        },
                    },
                    schema: PlanSchema::from_columns(vec![
                        ("id".to_owned(), DataType::Int64),
                        ("email".to_owned(), DataType::Text),
                    ]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![
                ("id".to_owned(), DataType::Int64),
                ("email".to_owned(), DataType::Text),
            ]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        match folded.node {
            PhysicalNode::Limit { input, .. } => match input.node {
                PhysicalNode::Db9Cop { ops, .. } => {
                    assert_eq!(ops.len(), 1);
                    assert!(matches!(ops[0], Db9CopOp::Limit { limit: 1 }));
                }
                other => panic!("expected Db9Cop under outer Limit, got {other:?}"),
            },
            other => panic!("expected outer Limit, got {other:?}"),
        }
    }

    #[test]
    fn safe_projection_over_timestamp_scan_still_folds() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![crate::sql::analyzer::types::AnalyzedProjection {
                    expr: TypedExpr::new(
                        TypedExprKind::ColumnRef {
                            scope_depth: 0,
                            column_index: 0,
                            column_name: "id".to_owned(),
                        },
                        DataType::Int64,
                    ),
                    output_name: "id".to_owned(),
                }],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "events".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![
                        ("id".to_owned(), DataType::Int64),
                        ("created_at".to_owned(), DataType::Timestamp),
                    ]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("id".to_owned(), DataType::Int64)]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        match folded.node {
            PhysicalNode::Db9Cop { ops, .. } => {
                assert_eq!(ops.len(), 1);
                assert!(matches!(ops[0], Db9CopOp::Project { .. }));
            }
            other => panic!("expected Db9Cop with safe projection, got {other:?}"),
        }
    }

    #[test]
    fn timestamp_filter_folds_when_timestamp_columns_are_supported() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![crate::sql::analyzer::types::AnalyzedProjection {
                    expr: TypedExpr::new(
                        TypedExprKind::ColumnRef {
                            scope_depth: 0,
                            column_index: 0,
                            column_name: "id".to_owned(),
                        },
                        DataType::Int64,
                    ),
                    output_name: "id".to_owned(),
                }],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::Filter {
                        predicate: TypedExpr::new(
                            TypedExprKind::IsTest {
                                expr: Box::new(TypedExpr::new(
                                    TypedExprKind::ColumnRef {
                                        scope_depth: 0,
                                        column_index: 1,
                                        column_name: "created_at".to_owned(),
                                    },
                                    DataType::Timestamp,
                                )),
                                test: IsTestKind::Null,
                                negated: false,
                            },
                            DataType::Boolean,
                        ),
                        input: Box::new(PhysicalPlan {
                            node: PhysicalNode::SeqScan {
                                table_name: "events".to_owned(),
                                alias: None,
                            },
                            schema: PlanSchema::from_columns(vec![
                                ("id".to_owned(), DataType::Int64),
                                ("created_at".to_owned(), DataType::Timestamp),
                            ]),
                            cost: PhysicalCost::default(),
                        }),
                    },
                    schema: PlanSchema::from_columns(vec![
                        ("id".to_owned(), DataType::Int64),
                        ("created_at".to_owned(), DataType::Timestamp),
                    ]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("id".to_owned(), DataType::Int64)]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        match folded.node {
            PhysicalNode::Db9Cop { ops, .. } => {
                assert_eq!(ops.len(), 2);
                assert!(matches!(ops[0], Db9CopOp::Filter { .. }));
                assert!(matches!(ops[1], Db9CopOp::Project { .. }));
            }
            other => panic!("expected Db9Cop with timestamp filter support, got {other:?}"),
        }
    }

    #[test]
    fn timestamp_equality_filter_folds_to_db9_cop() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![crate::sql::analyzer::types::AnalyzedProjection {
                    expr: TypedExpr::new(
                        TypedExprKind::ColumnRef {
                            scope_depth: 0,
                            column_index: 0,
                            column_name: "id".to_owned(),
                        },
                        DataType::Int64,
                    ),
                    output_name: "id".to_owned(),
                }],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::Filter {
                        predicate: TypedExpr::new(
                            TypedExprKind::BinaryOp {
                                left: Box::new(TypedExpr::new(
                                    TypedExprKind::ColumnRef {
                                        scope_depth: 0,
                                        column_index: 1,
                                        column_name: "created_at".to_owned(),
                                    },
                                    DataType::Timestamp,
                                )),
                                op: BinaryOp::Eq,
                                right: Box::new(TypedExpr::new(
                                    TypedExprKind::Constant(Value::Timestamp(1_700_000_000_000)),
                                    DataType::Timestamp,
                                )),
                            },
                            DataType::Boolean,
                        ),
                        input: Box::new(PhysicalPlan {
                            node: PhysicalNode::SeqScan {
                                table_name: "events".to_owned(),
                                alias: None,
                            },
                            schema: PlanSchema::from_columns(vec![
                                ("id".to_owned(), DataType::Int64),
                                ("created_at".to_owned(), DataType::Timestamp),
                            ]),
                            cost: PhysicalCost::default(),
                        }),
                    },
                    schema: PlanSchema::from_columns(vec![
                        ("id".to_owned(), DataType::Int64),
                        ("created_at".to_owned(), DataType::Timestamp),
                    ]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("id".to_owned(), DataType::Int64)]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        match folded.node {
            PhysicalNode::Db9Cop { ops, .. } => {
                assert_eq!(ops.len(), 2);
                assert!(matches!(ops[0], Db9CopOp::Filter { .. }));
                assert!(matches!(ops[1], Db9CopOp::Project { .. }));
            }
            other => panic!("expected Db9Cop with timestamp equality filter, got {other:?}"),
        }
    }

    #[test]
    fn timestamptz_equality_filter_folds_to_db9_cop() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![crate::sql::analyzer::types::AnalyzedProjection {
                    expr: TypedExpr::new(
                        TypedExprKind::ColumnRef {
                            scope_depth: 0,
                            column_index: 0,
                            column_name: "id".to_owned(),
                        },
                        DataType::Int64,
                    ),
                    output_name: "id".to_owned(),
                }],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::Filter {
                        predicate: TypedExpr::new(
                            TypedExprKind::BinaryOp {
                                left: Box::new(TypedExpr::new(
                                    TypedExprKind::ColumnRef {
                                        scope_depth: 0,
                                        column_index: 1,
                                        column_name: "created_tz".to_owned(),
                                    },
                                    DataType::TimestampTz,
                                )),
                                op: BinaryOp::Eq,
                                right: Box::new(TypedExpr::new(
                                    TypedExprKind::Constant(Value::Timestamp(1_700_000_000_000)),
                                    DataType::TimestampTz,
                                )),
                            },
                            DataType::Boolean,
                        ),
                        input: Box::new(PhysicalPlan {
                            node: PhysicalNode::SeqScan {
                                table_name: "events".to_owned(),
                                alias: None,
                            },
                            schema: PlanSchema::from_columns(vec![
                                ("id".to_owned(), DataType::Int64),
                                ("created_tz".to_owned(), DataType::TimestampTz),
                            ]),
                            cost: PhysicalCost::default(),
                        }),
                    },
                    schema: PlanSchema::from_columns(vec![
                        ("id".to_owned(), DataType::Int64),
                        ("created_tz".to_owned(), DataType::TimestampTz),
                    ]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("id".to_owned(), DataType::Int64)]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        match folded.node {
            PhysicalNode::Db9Cop { ops, .. } => {
                assert_eq!(ops.len(), 2);
                assert!(matches!(ops[0], Db9CopOp::Filter { .. }));
                assert!(matches!(ops[1], Db9CopOp::Project { .. }));
            }
            other => panic!("expected Db9Cop with timestamptz equality filter, got {other:?}"),
        }
    }
}
