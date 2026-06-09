use crate::model::{DataType, Value};
use crate::sql::analyzer::types::{
    AnalyzedProjection, BinaryOp, FunctionKind, IsTestKind, TypedExpr, TypedExprKind, UnaryOp,
};
use crate::sql::expr::functions::regex::translate_pg_regex_escapes;
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

const DB9_COP_MAX_TO_CHAR_PATTERN_BYTES: usize = 32 * 1024 * 1024;

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

fn eligible_db9_cop_index_scan(_scan_type: &ScanType) -> bool {
    // M0 pushdown wave: DB9 Cop only supports SeqScan-based execution on this
    // exact pair.
    //
    // Index scans imply base-table row fetch, and DB9 Cop cannot safely
    // orchestrate cross-region fetches in a single cop task. Index-only /
    // covering scans and late materialization are tracked as M3 work.
    false
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
        .all(|(_, data_type)| db9_cop_output_column_type_supported(data_type))
}

fn db9_cop_output_column_type_supported(data_type: &DataType) -> bool {
    db9_cop_scalar_output_column_type_supported(data_type)
        || matches!(
            data_type,
            // Scalar interval output has a dedicated wire carrier, but interval[]
            // still lacks an array-element carrier on this exact pair.
            DataType::Array(elem_type) if db9_cop_array_projection_type_supported(elem_type)
        )
}

fn db9_cop_scalar_output_column_type_supported(data_type: &DataType) -> bool {
    matches!(
        data_type,
        DataType::Boolean
            | DataType::Int32
            | DataType::Int64
            | DataType::Float64
            | DataType::Numeric { .. }
            | DataType::Text
            | DataType::Bytes
            | DataType::Date
            | DataType::Time
            | DataType::Timestamp
            | DataType::TimestampTz
            | DataType::Interval
            | DataType::Name
            | DataType::Varchar(_)
    )
}

fn db9_cop_input_column_type_supported(data_type: &DataType) -> bool {
    db9_cop_scalar_input_column_type_supported(data_type)
        || db9_cop_array_type_supported(data_type)
        || db9_cop_json_type_supported(data_type)
}

fn db9_cop_scalar_input_column_type_supported(data_type: &DataType) -> bool {
    matches!(
        data_type,
        DataType::Boolean
            | DataType::Int32
            | DataType::Int64
            | DataType::Float64
            | DataType::Text
            | DataType::Bytes
            | DataType::Date
            | DataType::Time
            | DataType::Timestamp
            | DataType::TimestampTz
            | DataType::Interval
            | DataType::Name
            | DataType::Varchar(_)
    )
}

fn db9_cop_expr_supported(expr: &TypedExpr) -> bool {
    match &expr.kind {
        TypedExprKind::Constant(value) => db9_cop_constant_supported(value, &expr.data_type),
        TypedExprKind::ColumnRef { scope_depth, .. } => {
            *scope_depth == 0 && db9_cop_input_column_type_supported(&expr.data_type)
        }
        TypedExprKind::BinaryOp { op, .. } if db9_cop_regex_binary_op(op) => false,
        TypedExprKind::BinaryOp { left, op, right } => {
            db9_cop_binary_op_supported(op, &left.data_type, &right.data_type, &expr.data_type)
                && db9_cop_expr_supported(left)
                && db9_cop_expr_supported(right)
        }
        TypedExprKind::UnaryOp { op, operand } => {
            db9_cop_unary_op_supported(op, &operand.data_type) && db9_cop_expr_supported(operand)
        }
        TypedExprKind::IsTest { expr, test, .. } => {
            db9_cop_is_test_supported(*test, expr) && db9_cop_expr_supported(expr)
        }
        TypedExprKind::IsDistinctFrom { left, right, .. } => {
            if left.is_null_constant() && right.is_null_constant() {
                true
            } else if left.is_null_constant() || right.is_null_constant() {
                let operand = if left.is_null_constant() { right } else { left };
                db9_cop_is_test_supported(IsTestKind::Null, operand)
                    && db9_cop_expr_supported(operand)
            } else {
                db9_cop_binary_op_supported(
                    &BinaryOp::Eq,
                    &left.data_type,
                    &right.data_type,
                    &DataType::Boolean,
                ) && db9_cop_binary_op_supported(
                    &BinaryOp::NotEq,
                    &left.data_type,
                    &right.data_type,
                    &DataType::Boolean,
                ) && db9_cop_expr_supported(left)
                    && db9_cop_expr_supported(right)
            }
        }
        TypedExprKind::Between {
            expr, low, high, ..
        } => {
            db9_cop_binary_op_supported(
                &BinaryOp::GtEq,
                &expr.data_type,
                &low.data_type,
                &DataType::Boolean,
            ) && db9_cop_binary_op_supported(
                &BinaryOp::LtEq,
                &expr.data_type,
                &high.data_type,
                &DataType::Boolean,
            ) && db9_cop_binary_op_supported(
                &BinaryOp::Lt,
                &expr.data_type,
                &low.data_type,
                &DataType::Boolean,
            ) && db9_cop_binary_op_supported(
                &BinaryOp::Gt,
                &expr.data_type,
                &high.data_type,
                &DataType::Boolean,
            ) && db9_cop_expr_supported(expr)
                && db9_cop_expr_supported(low)
                && db9_cop_expr_supported(high)
        }
        TypedExprKind::InList { expr, list, .. } => {
            !list.is_empty()
                && list
                    .iter()
                    .all(|item| !matches!(item.kind, TypedExprKind::Constant(Value::Null)))
                && list.iter().all(|item| {
                    db9_cop_binary_op_supported(
                        &BinaryOp::Eq,
                        &expr.data_type,
                        &item.data_type,
                        &DataType::Boolean,
                    ) && db9_cop_binary_op_supported(
                        &BinaryOp::NotEq,
                        &expr.data_type,
                        &item.data_type,
                        &DataType::Boolean,
                    ) && db9_cop_expr_supported(item)
                })
                && db9_cop_expr_supported(expr)
        }
        TypedExprKind::FunctionCall {
            func,
            args,
            order_by,
            filter,
        } => {
            filter.is_none()
                && order_by.is_empty()
                && matches!(func.kind, FunctionKind::Builtin)
                && db9_cop_builtin_function_supported(&func.name, args, &expr.data_type)
                && args.iter().all(db9_cop_expr_supported)
        }
        TypedExprKind::Coalesce(exprs) => {
            !exprs.is_empty() && exprs.iter().all(db9_cop_expr_supported)
        }
        TypedExprKind::NullIf(left, right) => {
            db9_cop_equality_comparison_supported(&left.data_type, &right.data_type)
                && db9_cop_expr_supported(left)
                && db9_cop_expr_supported(right)
        }
        TypedExprKind::ArrayLiteral(elems) => match &expr.data_type {
            DataType::Array(elem_type) => {
                db9_cop_array_projection_type_supported(elem_type)
                    && elems.iter().all(db9_cop_array_literal_elem_supported)
            }
            _ => false,
        },
        TypedExprKind::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            db9_cop_text_like_type_supported(&expr.data_type)
                && db9_cop_text_like_type_supported(&pattern.data_type)
                && db9_cop_expr_supported(expr)
                && db9_cop_expr_supported(pattern)
                && escape.as_ref().is_none_or(|escape| {
                    db9_cop_text_like_type_supported(&escape.data_type)
                        && db9_cop_expr_supported(escape)
                })
        }
        // DB9 Cop does not carry collation metadata on the wire. Any
        // explicit COLLATE wrapper must stay local until the protocol can carry
        // the resolved collation through both filter and projection paths.
        TypedExprKind::Collate { .. } => false,
        _ => false,
    }
}

fn db9_cop_is_test_supported(test: IsTestKind, expr: &TypedExpr) -> bool {
    match test {
        IsTestKind::Null => true,
        IsTestKind::True | IsTestKind::False | IsTestKind::Unknown => {
            matches!(expr.data_type, DataType::Boolean)
        }
    }
}

fn db9_cop_constant_supported(value: &Value, data_type: &DataType) -> bool {
    match value {
        Value::Null => db9_cop_null_constant_type_supported(data_type),
        Value::Boolean(_) => matches!(data_type, DataType::Boolean),
        Value::Int32(_) => matches!(data_type, DataType::Int32),
        Value::Int64(_) => matches!(data_type, DataType::Int64 | DataType::Oid),
        Value::Float64(_) => matches!(data_type, DataType::Float64),
        Value::Text(_) => {
            db9_cop_text_like_type_supported(data_type) || matches!(data_type, DataType::Unknown)
        }
        Value::Bytes(_) => matches!(data_type, DataType::Bytes),
        Value::Timestamp(_) => matches!(data_type, DataType::Timestamp | DataType::TimestampTz),
        _ => false,
    }
}

fn db9_cop_null_constant_type_supported(data_type: &DataType) -> bool {
    match data_type {
        DataType::Unknown | DataType::Vector(_) => false,
        DataType::Array(elem_type) => db9_cop_null_constant_type_supported(elem_type),
        _ => true,
    }
}

fn db9_cop_array_literal_constant_wire_supported(value: &Value, data_type: &DataType) -> bool {
    matches!(
        value,
        Value::Null
            | Value::Boolean(_)
            | Value::Int32(_)
            | Value::Int64(_)
            | Value::Float64(_)
            | Value::Text(_)
            | Value::Bytes(_)
    ) || matches!(
        (value, data_type),
        (
            Value::Timestamp(_),
            DataType::Timestamp | DataType::TimestampTz
        )
    )
}

fn db9_cop_array_literal_elem_supported(expr: &TypedExpr) -> bool {
    match &expr.kind {
        TypedExprKind::Constant(value) => {
            db9_cop_array_literal_constant_supported(value, &expr.data_type)
        }
        TypedExprKind::ArrayLiteral(_) => db9_cop_expr_supported(expr),
        _ => db9_cop_expr_supported(expr),
    }
}

fn db9_cop_array_literal_constant_supported(value: &Value, data_type: &DataType) -> bool {
    db9_cop_array_projection_type_supported(data_type)
        && db9_cop_array_literal_constant_wire_supported(value, data_type)
}

fn db9_cop_binary_op_supported(
    op: &BinaryOp,
    left_type: &DataType,
    right_type: &DataType,
    return_type: &DataType,
) -> bool {
    match op {
        BinaryOp::Eq | BinaryOp::NotEq => {
            matches!(return_type, DataType::Boolean)
                && db9_cop_equality_operand_types_supported(left_type, right_type)
        }
        BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq => {
            matches!(return_type, DataType::Boolean)
                && db9_cop_ordered_operand_types_supported(left_type, right_type)
        }
        BinaryOp::And | BinaryOp::Or => {
            matches!(return_type, DataType::Boolean)
                && matches!(
                    (left_type, right_type),
                    (DataType::Boolean, DataType::Boolean)
                )
        }
        BinaryOp::RegexMatch | BinaryOp::RegexNotMatch => false,
        BinaryOp::BitwiseAnd | BinaryOp::BitwiseOr | BinaryOp::BitwiseXor => {
            db9_cop_bitwise_operand_type_supported(left_type)
                && db9_cop_bitwise_operand_type_supported(right_type)
                && left_type == right_type
                && return_type == left_type
        }
        BinaryOp::ShiftLeft | BinaryOp::ShiftRight => {
            db9_cop_bitwise_operand_type_supported(left_type)
                // PostgreSQL shift signatures use int4 for the shift count:
                // int4 << int4 -> int4, int8 << int4 -> int8.
                && matches!(right_type, DataType::Int32)
                && return_type == left_type
        }
        _ => false,
    }
}

fn db9_cop_equality_comparison_supported(left_type: &DataType, right_type: &DataType) -> bool {
    db9_cop_binary_op_supported(&BinaryOp::Eq, left_type, right_type, &DataType::Boolean)
        && db9_cop_binary_op_supported(&BinaryOp::NotEq, left_type, right_type, &DataType::Boolean)
}

fn db9_cop_regex_binary_op(op: &BinaryOp) -> bool {
    matches!(op, BinaryOp::RegexMatch | BinaryOp::RegexNotMatch)
}

fn db9_cop_unary_op_supported(op: &UnaryOp, operand_type: &DataType) -> bool {
    match op {
        UnaryOp::Not => matches!(operand_type, DataType::Boolean),
        UnaryOp::Minus | UnaryOp::Plus => db9_cop_primitive_numeric_type_supported(operand_type),
        UnaryOp::BitwiseNot => db9_cop_bitwise_operand_type_supported(operand_type),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Db9CopBuiltinPolicy {
    PushdownSafe,
    LocalOnly,
    Unsupported,
}

fn db9_cop_builtin_function_supported(
    name: &str,
    args: &[TypedExpr],
    return_type: &DataType,
) -> bool {
    matches!(
        db9_cop_builtin_function_policy(name, args, return_type),
        Db9CopBuiltinPolicy::PushdownSafe
    )
}

fn db9_cop_builtin_function_policy(
    name: &str,
    args: &[TypedExpr],
    return_type: &DataType,
) -> Db9CopBuiltinPolicy {
    match name.to_ascii_lowercase().as_str() {
        "lower" | "upper" => {
            if args.len() == 1
                && db9_cop_text_like_type_supported(&args[0].data_type)
                && db9_cop_text_like_type_supported(return_type)
            {
                Db9CopBuiltinPolicy::LocalOnly
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "length" => {
            if args.len() == 1
                && db9_cop_length_input_type_supported(&args[0].data_type)
                && matches!(return_type, DataType::Int32)
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "char_length" | "character_length" => {
            if args.len() == 1
                && db9_cop_text_like_type_supported(&args[0].data_type)
                && matches!(return_type, DataType::Int32)
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "abs" => {
            if args.len() == 1
                && db9_cop_primitive_numeric_type_supported(&args[0].data_type)
                && db9_cop_primitive_numeric_type_supported(return_type)
                && &args[0].data_type == return_type
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "ceil" | "ceiling" | "floor" => {
            if args.len() == 1
                && db9_cop_primitive_numeric_type_supported(&args[0].data_type)
                && matches!(return_type, DataType::Float64)
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "round" | "trunc" => {
            if args.len() == 1
                && db9_cop_primitive_numeric_type_supported(&args[0].data_type)
                && matches!(return_type, DataType::Float64)
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "sqrt" | "exp" | "ln" | "log" | "log10" | "cbrt" | "degrees" | "radians" | "sin"
        | "cos" | "tan" | "atan" | "asin" | "acos" => {
            if args.len() == 1
                && db9_cop_float_math_arg_supported(&args[0].data_type)
                && matches!(return_type, DataType::Float64)
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "atan2" | "power" | "pow" => {
            if args.len() == 2
                && args
                    .iter()
                    .all(|arg| db9_cop_float_math_arg_supported(&arg.data_type))
                && matches!(return_type, DataType::Float64)
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "mod" => {
            if args.len() == 2
                && db9_cop_primitive_numeric_type_supported(&args[0].data_type)
                && db9_cop_primitive_numeric_type_supported(&args[1].data_type)
                && matches!(
                    (&args[0].data_type, &args[1].data_type, return_type),
                    (DataType::Int32, DataType::Int32, DataType::Int32)
                        | (DataType::Int32, DataType::Int64, DataType::Int64)
                        | (DataType::Int64, DataType::Int32, DataType::Int64)
                        | (DataType::Int64, DataType::Int64, DataType::Int64)
                )
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "sign" => {
            if args.len() == 1
                && db9_cop_sign_input_type_supported(&args[0].data_type)
                && db9_cop_sign_return_type_supported(&args[0].data_type, return_type)
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "pi" => {
            if args.is_empty() && matches!(return_type, DataType::Float64) {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "width_bucket" => {
            if args.len() == 4
                && args[..3]
                    .iter()
                    .all(|arg| db9_cop_numeric_type_supported(&arg.data_type))
                && db9_cop_round_precision_type_supported(&args[3].data_type)
                && matches!(return_type, DataType::Int32)
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "hashtext" => {
            if args.len() == 1
                && db9_cop_text_like_type_supported(&args[0].data_type)
                && matches!(return_type, DataType::Int32)
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "extract" => {
            if args.len() == 2
                && db9_cop_text_constant_supported(&args[0])
                && db9_cop_temporal_date_part_field_supported(&args[0])
                && matches!(args[1].data_type, DataType::Timestamp)
                && matches!(return_type, DataType::Numeric { .. })
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "date_part" => {
            if args.len() == 2 && db9_cop_text_constant_supported(&args[0]) {
                let source_type = &args[1].data_type;
                let pushes_timestamp = db9_cop_temporal_date_part_field_supported(&args[0])
                    && matches!(source_type, DataType::Timestamp)
                    && matches!(return_type, DataType::Float64);
                if pushes_timestamp {
                    Db9CopBuiltinPolicy::PushdownSafe
                } else {
                    Db9CopBuiltinPolicy::Unsupported
                }
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "date_trunc" => {
            if args.len() == 2
                && db9_cop_text_constant_supported(&args[0])
                && db9_cop_date_trunc_field_supported(&args[0])
                && matches!(args[1].data_type, DataType::Timestamp)
                && matches!(return_type, DataType::Timestamp)
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "to_char" => {
            if args.len() == 2
                && matches!(args[0].data_type, DataType::Timestamp)
                && db9_cop_to_char_pattern_supported(&args[1])
                && matches!(return_type, DataType::Text)
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "date" => {
            if args.len() == 1
                && matches!(args[0].data_type, DataType::TimestampTz)
                && matches!(return_type, DataType::Date)
            {
                Db9CopBuiltinPolicy::LocalOnly
            } else if args.len() == 1
                && db9_cop_date_function_input_type_supported(&args[0].data_type)
                && matches!(return_type, DataType::Date)
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "age" => {
            if args.len() == 2
                && args
                    .iter()
                    .all(|arg| db9_cop_age_input_type_supported(&arg.data_type))
                && matches!(return_type, DataType::Interval)
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "make_date" => {
            if args.len() == 3
                && args
                    .iter()
                    .all(|arg| db9_cop_int32_type_supported(&arg.data_type))
                && matches!(return_type, DataType::Date)
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "make_time" => {
            if args.len() == 3
                && db9_cop_int32_type_supported(&args[0].data_type)
                && db9_cop_int32_type_supported(&args[1].data_type)
                && db9_cop_primitive_numeric_arg_supported(&args[2])
                && matches!(return_type, DataType::Time)
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "make_timestamp" => {
            if args.len() == 6
                && args[..5]
                    .iter()
                    .all(|arg| db9_cop_int32_type_supported(&arg.data_type))
                && db9_cop_primitive_numeric_arg_supported(&args[5])
                && matches!(return_type, DataType::Timestamp)
            {
                Db9CopBuiltinPolicy::LocalOnly
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "make_interval" => {
            if args.len() <= 7
                && args
                    .iter()
                    .take(6)
                    .all(|arg| db9_cop_int32_type_supported(&arg.data_type))
                && args
                    .get(6)
                    .is_none_or(db9_cop_primitive_numeric_arg_supported)
                && matches!(return_type, DataType::Interval)
            {
                Db9CopBuiltinPolicy::LocalOnly
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "to_timestamp" => {
            if args.len() == 1
                && db9_cop_primitive_numeric_arg_supported(&args[0])
                && matches!(return_type, DataType::TimestampTz)
            {
                Db9CopBuiltinPolicy::LocalOnly
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "regexp_split_to_array" => {
            if matches!(args.len(), 2 | 3)
                && args[..2]
                    .iter()
                    .all(|arg| db9_cop_text_like_type_supported(&arg.data_type))
                && args
                    .get(2)
                    .is_none_or(|arg| db9_cop_text_like_type_supported(&arg.data_type))
                && db9_cop_regex_pattern_expr_supported(&args[1], args.get(2), true)
                && db9_cop_text_array_type_supported(return_type)
            {
                Db9CopBuiltinPolicy::LocalOnly
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "array_length" | "array_upper" | "array_lower" => {
            if args.len() == 2
                && db9_cop_array_type_supported(&args[0].data_type)
                && db9_cop_array_dimension_type_supported(&args[1].data_type)
                && matches!(return_type, DataType::Int32)
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "cardinality" => {
            if args.len() == 1
                && db9_cop_array_type_supported(&args[0].data_type)
                && matches!(return_type, DataType::Int32)
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "array_position" => {
            if args.len() == 2
                && db9_cop_one_dimensional_array_eq_type_supported(&args[0].data_type)
                && db9_cop_array_element_type_matches(&args[0].data_type, &args[1].data_type)
                && matches!(return_type, DataType::Int32)
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "__db9_eq_any" => {
            if args.len() == 2
                && db9_cop_one_dimensional_array_type_supported(&args[0].data_type)
                && db9_cop_array_eq_any_types_supported(&args[0].data_type, &args[1].data_type)
                && matches!(return_type, DataType::Boolean)
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "array_cat" => {
            if args.len() == 2
                && db9_cop_same_one_dimensional_array_type(&args[0].data_type, &args[1].data_type)
                && &args[0].data_type == return_type
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "array_append" => {
            if args.len() == 2
                && db9_cop_one_dimensional_array_type_supported(&args[0].data_type)
                && db9_cop_array_element_type_matches(&args[0].data_type, &args[1].data_type)
                && &args[0].data_type == return_type
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "array_remove" => {
            if args.len() == 2
                && db9_cop_one_dimensional_array_eq_type_supported(&args[0].data_type)
                && db9_cop_array_element_type_matches(&args[0].data_type, &args[1].data_type)
                && &args[0].data_type == return_type
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "array_prepend" => {
            if args.len() == 2
                && db9_cop_one_dimensional_array_type_supported(&args[1].data_type)
                && db9_cop_array_element_type_matches(&args[1].data_type, &args[0].data_type)
                && &args[1].data_type == return_type
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "array_to_string" => {
            if matches!(args.len(), 2 | 3)
                && db9_cop_array_string_renderable_type_supported(&args[0].data_type)
                && db9_cop_text_like_type_supported(&args[1].data_type)
                && args
                    .get(2)
                    .is_none_or(|arg| db9_cop_text_like_type_supported(&arg.data_type))
                && db9_cop_text_like_type_supported(return_type)
            {
                Db9CopBuiltinPolicy::LocalOnly
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "string_to_array" => {
            if matches!(args.len(), 2 | 3)
                && args[..2]
                    .iter()
                    .all(|arg| db9_cop_text_like_type_supported(&arg.data_type))
                && args
                    .get(2)
                    .is_none_or(|arg| db9_cop_text_like_type_supported(&arg.data_type))
                && db9_cop_text_array_type_supported(return_type)
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "concat" => {
            if !args.is_empty()
                && args.iter().all(|arg| {
                    db9_cop_non_temporal_string_renderable_type_supported(&arg.data_type)
                })
                && db9_cop_text_like_type_supported(return_type)
            {
                Db9CopBuiltinPolicy::LocalOnly
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "concat_ws" => {
            if args.len() >= 2
                && args.iter().all(|arg| {
                    db9_cop_non_temporal_string_renderable_type_supported(&arg.data_type)
                })
                && db9_cop_text_like_type_supported(return_type)
            {
                Db9CopBuiltinPolicy::LocalOnly
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "left" | "right" => {
            if args.len() == 2
                && db9_cop_text_like_type_supported(&args[0].data_type)
                && db9_cop_int32_type_supported(&args[1].data_type)
                && db9_cop_text_like_type_supported(return_type)
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "repeat" => {
            if args.len() == 2
                && db9_cop_text_like_type_supported(&args[0].data_type)
                && db9_cop_int32_type_supported(&args[1].data_type)
                && db9_cop_text_like_type_supported(return_type)
            {
                Db9CopBuiltinPolicy::LocalOnly
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "reverse" => {
            if args.len() == 1
                && db9_cop_text_like_type_supported(&args[0].data_type)
                && db9_cop_text_like_type_supported(return_type)
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "initcap" => {
            if args.len() == 1
                && db9_cop_text_like_type_supported(&args[0].data_type)
                && db9_cop_text_like_type_supported(return_type)
            {
                Db9CopBuiltinPolicy::LocalOnly
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "ascii" => {
            if args.len() == 1
                && db9_cop_text_like_type_supported(&args[0].data_type)
                && matches!(return_type, DataType::Int32)
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "chr" => {
            if args.len() == 1
                && db9_cop_int32_type_supported(&args[0].data_type)
                && db9_cop_text_like_type_supported(return_type)
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "substring" | "substr" => {
            if matches!(args.len(), 2 | 3)
                && db9_cop_string_or_bytes_type_supported(&args[0].data_type)
                && db9_cop_int32_type_supported(&args[1].data_type)
                && args
                    .get(2)
                    .is_none_or(|arg| db9_cop_int32_type_supported(&arg.data_type))
                && db9_cop_same_string_family_return_type(&args[0].data_type, return_type)
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "btrim" | "ltrim" | "rtrim" => {
            if matches!(args.len(), 1 | 2)
                && db9_cop_text_like_type_supported(&args[0].data_type)
                && args
                    .get(1)
                    .is_none_or(|arg| db9_cop_text_like_type_supported(&arg.data_type))
                && db9_cop_text_like_type_supported(return_type)
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "lpad" | "rpad" => {
            if matches!(args.len(), 2 | 3)
                && db9_cop_text_like_type_supported(&args[0].data_type)
                && db9_cop_int32_type_supported(&args[1].data_type)
                && args
                    .get(2)
                    .is_none_or(|arg| db9_cop_text_like_type_supported(&arg.data_type))
                && db9_cop_text_like_type_supported(return_type)
            {
                Db9CopBuiltinPolicy::LocalOnly
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "replace" | "translate" => {
            if args.len() == 3
                && db9_cop_text_like_type_supported(&args[0].data_type)
                && db9_cop_text_like_type_supported(&args[1].data_type)
                && db9_cop_text_like_type_supported(&args[2].data_type)
                && db9_cop_text_like_type_supported(return_type)
            {
                Db9CopBuiltinPolicy::LocalOnly
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "strpos" => {
            if args.len() == 2
                && db9_cop_text_like_type_supported(&args[0].data_type)
                && db9_cop_text_like_type_supported(&args[1].data_type)
                && matches!(return_type, DataType::Int32)
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "starts_with" | "to_hex" => Db9CopBuiltinPolicy::LocalOnly,
        "split_part" => {
            if args.len() == 3
                && db9_cop_text_like_type_supported(&args[0].data_type)
                && db9_cop_text_like_type_supported(&args[1].data_type)
                && db9_cop_int32_type_supported(&args[2].data_type)
                && db9_cop_text_like_type_supported(return_type)
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "quote_ident" => {
            if args.len() == 1
                && db9_cop_text_like_type_supported(&args[0].data_type)
                && db9_cop_text_like_type_supported(return_type)
            {
                Db9CopBuiltinPolicy::LocalOnly
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "quote_literal" | "quote_nullable" => {
            if args.len() == 1
                && db9_cop_quote_string_renderable_type_supported(&args[0].data_type)
                && db9_cop_text_like_type_supported(return_type)
            {
                Db9CopBuiltinPolicy::LocalOnly
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "overlay" => {
            if matches!(args.len(), 3 | 4)
                && db9_cop_overlay_input_types_supported(&args[0].data_type, &args[1].data_type)
                && db9_cop_int32_type_supported(&args[2].data_type)
                && args
                    .get(3)
                    .is_none_or(|arg| db9_cop_int32_type_supported(&arg.data_type))
                && db9_cop_same_string_family_return_type(&args[0].data_type, return_type)
            {
                Db9CopBuiltinPolicy::LocalOnly
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "format" => {
            if !args.is_empty()
                && args.iter().all(|arg| {
                    db9_cop_non_temporal_string_renderable_type_supported(&arg.data_type)
                })
                && db9_cop_text_like_type_supported(return_type)
            {
                Db9CopBuiltinPolicy::LocalOnly
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "md5" => {
            if args.len() == 1
                && db9_cop_hash_data_type_supported(&args[0].data_type)
                && db9_cop_text_like_type_supported(return_type)
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "sha256" => {
            if args.len() == 1
                && matches!(args[0].data_type, DataType::Bytes)
                && matches!(return_type, DataType::Bytes)
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "digest" => {
            if args.len() == 2
                && db9_cop_hash_data_type_supported(&args[0].data_type)
                && db9_cop_text_like_type_supported(&args[1].data_type)
                && matches!(return_type, DataType::Bytes)
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "encode" => {
            if args.len() == 2
                && matches!(args[0].data_type, DataType::Bytes)
                && db9_cop_text_like_type_supported(&args[1].data_type)
                && db9_cop_text_like_type_supported(return_type)
            {
                Db9CopBuiltinPolicy::LocalOnly
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "decode" => {
            if args.len() == 2
                && db9_cop_text_like_type_supported(&args[0].data_type)
                && db9_cop_text_like_type_supported(&args[1].data_type)
                && matches!(return_type, DataType::Bytes)
            {
                Db9CopBuiltinPolicy::PushdownSafe
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "regexp_replace" => {
            if matches!(args.len(), 3 | 4)
                && args[..3]
                    .iter()
                    .all(|arg| db9_cop_text_like_type_supported(&arg.data_type))
                && args
                    .get(3)
                    .is_none_or(|arg| db9_cop_text_like_type_supported(&arg.data_type))
                && db9_cop_regex_pattern_expr_supported(&args[1], args.get(3), false)
                && db9_cop_text_like_type_supported(return_type)
            {
                Db9CopBuiltinPolicy::LocalOnly
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "regexp_match" => {
            if matches!(args.len(), 2 | 3)
                && args[..2]
                    .iter()
                    .all(|arg| db9_cop_text_like_type_supported(&arg.data_type))
                && args
                    .get(2)
                    .is_none_or(|arg| db9_cop_text_like_type_supported(&arg.data_type))
                && db9_cop_regex_pattern_expr_supported(&args[1], args.get(2), true)
                && db9_cop_text_array_type_supported(return_type)
            {
                Db9CopBuiltinPolicy::LocalOnly
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "json_array_length" | "jsonb_array_length" => {
            if args.len() == 1
                && db9_cop_json_input_type_supported(&args[0].data_type)
                && matches!(return_type, DataType::Int32)
            {
                Db9CopBuiltinPolicy::LocalOnly
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "json_typeof" | "jsonb_typeof" => {
            if args.len() == 1
                && db9_cop_json_input_type_supported(&args[0].data_type)
                && db9_cop_text_like_type_supported(return_type)
            {
                Db9CopBuiltinPolicy::LocalOnly
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "json_extract_path_text" | "jsonb_extract_path_text" => {
            if args.len() >= 2
                && db9_cop_json_input_type_supported(&args[0].data_type)
                && args[1..]
                    .iter()
                    .all(|arg| db9_cop_text_like_type_supported(&arg.data_type))
                && db9_cop_text_like_type_supported(return_type)
            {
                Db9CopBuiltinPolicy::LocalOnly
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "jsonb_pretty" => {
            if args.len() == 1
                && db9_cop_json_input_type_supported(&args[0].data_type)
                && db9_cop_text_like_type_supported(return_type)
            {
                Db9CopBuiltinPolicy::LocalOnly
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        "jsonb_exists" => {
            if args.len() == 2
                && db9_cop_json_input_type_supported(&args[0].data_type)
                && db9_cop_text_like_type_supported(&args[1].data_type)
                && matches!(return_type, DataType::Boolean)
            {
                Db9CopBuiltinPolicy::LocalOnly
            } else {
                Db9CopBuiltinPolicy::Unsupported
            }
        }
        // COALESCE / NULLIF are normalized by the analyzer into dedicated
        // TypedExprKind variants with type-safe coercions. Treat any raw
        // FunctionCall form as unsupported so stale/untyped call paths cannot
        // widen the pushed surface by accident.
        "coalesce" | "nullif" => Db9CopBuiltinPolicy::Unsupported,
        _ => Db9CopBuiltinPolicy::Unsupported,
    }
}

fn db9_cop_text_like_type_supported(data_type: &DataType) -> bool {
    matches!(
        data_type,
        DataType::Text | DataType::Name | DataType::Varchar(_)
    )
}

fn db9_cop_length_input_type_supported(data_type: &DataType) -> bool {
    db9_cop_text_like_type_supported(data_type) || matches!(data_type, DataType::Bytes)
}

fn db9_cop_primitive_numeric_type_supported(data_type: &DataType) -> bool {
    matches!(
        data_type,
        DataType::Int32 | DataType::Int64 | DataType::Float64
    )
}

fn db9_cop_primitive_numeric_arg_supported(expr: &TypedExpr) -> bool {
    db9_cop_primitive_numeric_type_supported(&expr.data_type)
}

fn db9_cop_numeric_type_supported(data_type: &DataType) -> bool {
    db9_cop_primitive_numeric_type_supported(data_type)
        || matches!(data_type, DataType::Numeric { .. })
}

fn db9_cop_float_math_arg_supported(data_type: &DataType) -> bool {
    db9_cop_primitive_numeric_type_supported(data_type)
}

fn db9_cop_round_precision_type_supported(data_type: &DataType) -> bool {
    matches!(data_type, DataType::Int32)
}

fn db9_cop_hash_data_type_supported(data_type: &DataType) -> bool {
    db9_cop_text_like_type_supported(data_type) || matches!(data_type, DataType::Bytes)
}

fn db9_cop_sign_input_type_supported(data_type: &DataType) -> bool {
    db9_cop_numeric_type_supported(data_type)
}

fn db9_cop_sign_return_type_supported(input_type: &DataType, return_type: &DataType) -> bool {
    match input_type {
        DataType::Int32 | DataType::Int64 | DataType::Float64 => {
            matches!(return_type, DataType::Float64)
        }
        DataType::Numeric { .. } => matches!(return_type, DataType::Numeric { .. }),
        _ => false,
    }
}

fn db9_cop_int32_type_supported(data_type: &DataType) -> bool {
    matches!(data_type, DataType::Int32)
}

fn db9_cop_bitwise_operand_type_supported(data_type: &DataType) -> bool {
    matches!(data_type, DataType::Int32 | DataType::Int64)
}

fn db9_cop_age_input_type_supported(data_type: &DataType) -> bool {
    matches!(data_type, DataType::Date | DataType::Timestamp)
}

fn db9_cop_date_function_input_type_supported(data_type: &DataType) -> bool {
    matches!(data_type, DataType::Date | DataType::Timestamp)
}

fn db9_cop_text_constant_supported(expr: &TypedExpr) -> bool {
    db9_cop_text_constant_value(expr).is_some()
}

fn db9_cop_text_constant_value(expr: &TypedExpr) -> Option<&str> {
    match &expr.kind {
        TypedExprKind::Constant(Value::Text(value)) => Some(value.as_str()),
        _ => None,
    }
}

fn db9_cop_regex_pattern_expr_supported(
    pattern_expr: &TypedExpr,
    flags_expr: Option<&TypedExpr>,
    reject_global: bool,
) -> bool {
    let Some(pattern) = db9_cop_text_constant_value(pattern_expr) else {
        return false;
    };
    let flags = match flags_expr {
        Some(expr) => match db9_cop_text_constant_value(expr) {
            Some(value) => Some(value),
            None => return false,
        },
        None => None,
    };
    db9_cop_rust_regex_pattern_supported(pattern, flags, reject_global)
}

fn db9_cop_rust_regex_pattern_supported(
    pattern: &str,
    flags: Option<&str>,
    reject_global: bool,
) -> bool {
    let mut case_insensitive = false;
    let mut extended = false;
    let mut quote_literal = false;

    if let Some(flags) = flags {
        for flag in flags.chars() {
            match flag {
                'b' | 'e' | 't' => {}
                'c' => case_insensitive = false,
                'i' => case_insensitive = true,
                'x' => extended = true,
                'q' => quote_literal = true,
                's' | 'n' | 'm' | 'p' | 'w' => {}
                'g' if !reject_global => {}
                'g' => return false,
                _ => return false,
            }
        }
    }

    if !quote_literal
        && (db9_cop_regex_pattern_has_rust_named_capture(pattern)
            || db9_cop_regex_pattern_has_pg_bracket_construct(pattern))
    {
        return false;
    }

    let pattern = if quote_literal {
        regex::escape(pattern)
    } else {
        let Ok(pattern) = translate_pg_regex_escapes(pattern) else {
            return false;
        };
        pattern
    };
    let mut inline_flags = String::new();
    if case_insensitive {
        inline_flags.push('i');
    }
    if extended {
        inline_flags.push('x');
    }
    let candidate = if inline_flags.is_empty() {
        pattern
    } else {
        format!("(?{inline_flags}){pattern}")
    };

    fancy_regex::Regex::new(&candidate).is_ok()
}

fn db9_cop_regex_pattern_has_rust_named_capture(pattern: &str) -> bool {
    pattern.contains("(?P<") || pattern.contains("(?<") || pattern.contains("(?'")
}

fn db9_cop_regex_pattern_has_pg_bracket_construct(pattern: &str) -> bool {
    let mut escaped = false;
    for (index, ch) in pattern.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == '[' {
            let rest = &pattern[index..];
            if rest.starts_with("[:") || rest.starts_with("[.") || rest.starts_with("[=") {
                return true;
            }
        }
    }
    false
}

fn db9_cop_temporal_date_part_field_supported(expr: &TypedExpr) -> bool {
    let Some(field) = db9_cop_text_constant_value(expr) else {
        return false;
    };
    let field = field.trim().to_ascii_lowercase();
    matches!(
        field.as_str(),
        "year"
            | "month"
            | "day"
            | "hour"
            | "minute"
            | "second"
            | "dow"
            | "doy"
            | "week"
            | "quarter"
            | "epoch"
    )
}

fn db9_cop_date_trunc_field_supported(expr: &TypedExpr) -> bool {
    let Some(field) = db9_cop_text_constant_value(expr) else {
        return false;
    };
    let field = field.trim().to_ascii_lowercase();
    matches!(
        field.as_str(),
        "year" | "month" | "day" | "hour" | "minute" | "second"
    )
}

fn db9_cop_to_char_pattern_supported(expr: &TypedExpr) -> bool {
    let Some(pattern) = db9_cop_text_constant_value(expr) else {
        return false;
    };
    if pattern.len() > DB9_COP_MAX_TO_CHAR_PATTERN_BYTES {
        return false;
    }

    let supported_tokens = ["HH24", "YYYY", "MM", "DD", "MI", "SS"];
    let mut i = 0;
    let mut in_quotes = false;
    let chars = pattern.as_bytes();
    while i < chars.len() {
        if chars[i] == b'"' {
            in_quotes = !in_quotes;
            i += 1;
            continue;
        }

        if in_quotes {
            i += 1;
            continue;
        }

        let rest = &pattern[i..];
        if let Some(token) = supported_tokens
            .iter()
            .find(|token| starts_with_to_char_token(rest, token))
        {
            i += token.len();
            continue;
        }

        let ch = rest.chars().next().expect("rest is non-empty");
        if ch.is_ascii_whitespace()
            || matches!(
                ch,
                '-' | '/' | ':' | '.' | ',' | '_' | '(' | ')' | '[' | ']' | '+'
            )
        {
            i += ch.len_utf8();
            continue;
        }

        return false;
    }

    true
}

fn starts_with_to_char_token(rest: &str, token: &str) -> bool {
    let rest = rest.as_bytes();
    let token = token.as_bytes();
    rest.len() >= token.len() && rest[..token.len()].eq_ignore_ascii_case(token)
}

fn db9_cop_json_type_supported(data_type: &DataType) -> bool {
    matches!(data_type, DataType::Json | DataType::Jsonb)
}

fn db9_cop_json_input_type_supported(data_type: &DataType) -> bool {
    db9_cop_json_type_supported(data_type) || db9_cop_text_like_type_supported(data_type)
}

fn db9_cop_array_projection_type_supported(data_type: &DataType) -> bool {
    match data_type {
        DataType::Array(elem_type) => db9_cop_array_projection_type_supported(elem_type),
        DataType::Interval => false,
        _ => db9_cop_scalar_input_column_type_supported(data_type),
    }
}

fn db9_cop_text_array_type_supported(data_type: &DataType) -> bool {
    matches!(
        data_type,
        DataType::Array(elem_type) if db9_cop_text_like_type_supported(elem_type)
    )
}

fn db9_cop_array_type_supported(data_type: &DataType) -> bool {
    matches!(
        data_type,
        DataType::Array(elem_type) if db9_cop_array_projection_type_supported(elem_type)
    )
}

fn db9_cop_one_dimensional_array_type_supported(data_type: &DataType) -> bool {
    matches!(
        data_type,
        DataType::Array(elem_type) if !matches!(elem_type.as_ref(), DataType::Array(_))
    )
}

fn db9_cop_one_dimensional_array_eq_type_supported(data_type: &DataType) -> bool {
    db9_cop_one_dimensional_array_type_supported(data_type)
        && db9_cop_array_eq_type_supported(data_type)
}

fn db9_cop_array_dimension_type_supported(data_type: &DataType) -> bool {
    matches!(data_type, DataType::Int32)
}

fn db9_cop_array_element_type_matches(array_type: &DataType, elem_type: &DataType) -> bool {
    matches!(array_type, DataType::Array(array_elem_type) if array_elem_type.as_ref() == elem_type)
}

fn db9_cop_array_eq_any_types_supported(array_type: &DataType, needle_type: &DataType) -> bool {
    matches!(
        array_type,
        DataType::Array(array_elem_type)
            if db9_cop_equality_operand_types_supported(array_elem_type, needle_type)
    )
}

fn db9_cop_array_eq_type_supported(data_type: &DataType) -> bool {
    matches!(
        data_type,
        DataType::Array(elem_type) if db9_cop_array_eq_element_type_supported(elem_type)
    )
}

fn db9_cop_array_eq_element_type_supported(data_type: &DataType) -> bool {
    match data_type {
        DataType::Array(elem_type) => db9_cop_array_eq_element_type_supported(elem_type),
        _ => db9_cop_equality_family(data_type).is_some(),
    }
}

fn db9_cop_array_string_renderable_type_supported(data_type: &DataType) -> bool {
    matches!(
        data_type,
        DataType::Array(elem_type) if db9_cop_array_string_renderable_element_supported(elem_type)
    )
}

fn db9_cop_array_string_renderable_element_supported(data_type: &DataType) -> bool {
    db9_cop_string_renderable_type_supported(data_type)
        || matches!(
            data_type,
            DataType::Uuid | DataType::Json | DataType::Jsonb | DataType::Numeric { .. }
        )
}

fn db9_cop_string_renderable_type_supported(data_type: &DataType) -> bool {
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
            | DataType::Uuid
            | DataType::Json
            | DataType::Jsonb
            | DataType::Numeric { .. }
            | DataType::Name
            | DataType::Varchar(_)
    )
}

fn db9_cop_non_temporal_string_renderable_type_supported(data_type: &DataType) -> bool {
    matches!(
        data_type,
        DataType::Boolean
            | DataType::Int32
            | DataType::Int64
            | DataType::Float64
            | DataType::Text
            | DataType::Name
            | DataType::Varchar(_)
    )
}

fn db9_cop_quote_string_renderable_type_supported(data_type: &DataType) -> bool {
    db9_cop_non_temporal_string_renderable_type_supported(data_type)
}

fn db9_cop_string_or_bytes_type_supported(data_type: &DataType) -> bool {
    db9_cop_text_like_type_supported(data_type) || matches!(data_type, DataType::Bytes)
}

fn db9_cop_same_array_type(left_type: &DataType, right_type: &DataType) -> bool {
    matches!(
        (left_type, right_type),
        (DataType::Array(left_elem), DataType::Array(right_elem)) if left_elem == right_elem
    )
}

fn db9_cop_same_one_dimensional_array_type(left_type: &DataType, right_type: &DataType) -> bool {
    db9_cop_one_dimensional_array_type_supported(left_type)
        && db9_cop_same_array_type(left_type, right_type)
}

fn db9_cop_same_temporal_compare_type_supported(
    left_type: &DataType,
    right_type: &DataType,
) -> bool {
    matches!(
        (left_type, right_type),
        (DataType::Timestamp, DataType::Timestamp) | (DataType::TimestampTz, DataType::TimestampTz)
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Db9CopEqualityFamily {
    Boolean,
    PrimitiveNumeric,
    TextLike,
    Bytes,
    Timestamp,
    TimestampTz,
}

fn db9_cop_equality_family(data_type: &DataType) -> Option<Db9CopEqualityFamily> {
    if db9_cop_primitive_numeric_type_supported(data_type) {
        return Some(Db9CopEqualityFamily::PrimitiveNumeric);
    }
    if db9_cop_text_like_type_supported(data_type) {
        return Some(Db9CopEqualityFamily::TextLike);
    }
    match data_type {
        DataType::Boolean => Some(Db9CopEqualityFamily::Boolean),
        DataType::Bytes => Some(Db9CopEqualityFamily::Bytes),
        DataType::Timestamp => Some(Db9CopEqualityFamily::Timestamp),
        DataType::TimestampTz => Some(Db9CopEqualityFamily::TimestampTz),
        _ => None,
    }
}

fn db9_cop_equality_families_compatible(left_type: &DataType, right_type: &DataType) -> bool {
    matches!(
        (
            db9_cop_equality_family(left_type),
            db9_cop_equality_family(right_type),
        ),
        (
            Some(Db9CopEqualityFamily::Boolean),
            Some(Db9CopEqualityFamily::Boolean)
        ) | (
            Some(Db9CopEqualityFamily::PrimitiveNumeric),
            Some(Db9CopEqualityFamily::PrimitiveNumeric)
        ) | (
            Some(Db9CopEqualityFamily::TextLike),
            Some(Db9CopEqualityFamily::TextLike)
        ) | (
            Some(Db9CopEqualityFamily::Bytes),
            Some(Db9CopEqualityFamily::Bytes)
        ) | (
            Some(Db9CopEqualityFamily::Timestamp),
            Some(Db9CopEqualityFamily::Timestamp)
        ) | (
            Some(Db9CopEqualityFamily::TimestampTz),
            Some(Db9CopEqualityFamily::TimestampTz)
        )
    )
}

fn db9_cop_equality_operand_types_supported(left_type: &DataType, right_type: &DataType) -> bool {
    db9_cop_equality_families_compatible(left_type, right_type)
        || (db9_cop_same_array_type(left_type, right_type)
            && db9_cop_array_eq_type_supported(left_type))
}

fn db9_cop_ordered_operand_types_supported(left_type: &DataType, right_type: &DataType) -> bool {
    (db9_cop_primitive_numeric_type_supported(left_type)
        && db9_cop_primitive_numeric_type_supported(right_type))
        || (db9_cop_text_like_type_supported(left_type)
            && db9_cop_text_like_type_supported(right_type))
        || db9_cop_same_temporal_compare_type_supported(left_type, right_type)
}

fn db9_cop_same_string_family_return_type(input_type: &DataType, return_type: &DataType) -> bool {
    match input_type {
        DataType::Bytes => matches!(return_type, DataType::Bytes),
        _ => db9_cop_text_like_type_supported(return_type),
    }
}

fn db9_cop_overlay_input_types_supported(left_type: &DataType, right_type: &DataType) -> bool {
    matches!((left_type, right_type), (DataType::Bytes, DataType::Bytes))
        || (db9_cop_text_like_type_supported(left_type)
            && db9_cop_text_like_type_supported(right_type))
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
    if matches!(values[0], Value::Null) {
        return false;
    }

    let Some((column_name, value)) = single_column_equality_constant(predicate) else {
        return false;
    };
    if matches!(value, Value::Null) {
        return false;
    }

    column_name == lookup_column && value == values[0]
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
    if column_name != lookup_column {
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
    use crate::sql::analyzer::catalog::MockCatalog;
    use crate::sql::analyzer::scope::Scope;
    use crate::sql::analyzer::types::{ResolvedFunction, TypedExprKind};
    use crate::sql::analyzer::{AnalyzedQueryBody, Analyzer};
    use crate::sql::optimizer::logical_plan::PlanSchema;
    use crate::sql::types::CastContext;
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    fn builtin(name: &str, return_type: DataType) -> ResolvedFunction {
        ResolvedFunction {
            name: name.to_owned(),
            kind: FunctionKind::Builtin,
            return_type,
        }
    }

    fn builtin_call(name: &str, return_type: DataType, args: Vec<TypedExpr>) -> TypedExpr {
        TypedExpr::new(
            TypedExprKind::FunctionCall {
                func: builtin(name, return_type.clone()),
                args,
                order_by: vec![],
                filter: None,
            },
            return_type,
        )
    }

    fn column_ref(column_index: usize, column_name: &str, data_type: DataType) -> TypedExpr {
        TypedExpr::new(
            TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index,
                column_name: column_name.to_owned(),
            },
            data_type,
        )
    }

    fn text_constant(value: &str) -> TypedExpr {
        TypedExpr::new(
            TypedExprKind::Constant(Value::Text(value.to_owned())),
            DataType::Text,
        )
    }

    fn int32_constant(value: i32) -> TypedExpr {
        TypedExpr::new(
            TypedExprKind::Constant(Value::Int32(value)),
            DataType::Int32,
        )
    }

    fn binary_expr(
        op: BinaryOp,
        left_type: DataType,
        right_type: DataType,
        return_type: DataType,
    ) -> TypedExpr {
        TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(column_ref(0, "left_value", left_type)),
                op,
                right: Box::new(column_ref(1, "right_value", right_type)),
            },
            return_type,
        )
    }

    fn float64_constant(value: f64) -> TypedExpr {
        TypedExpr::new(
            TypedExprKind::Constant(Value::Float64(value)),
            DataType::Float64,
        )
    }

    fn timestamp_constant(value: i64) -> TypedExpr {
        TypedExpr::new(
            TypedExprKind::Constant(Value::Timestamp(value)),
            DataType::Timestamp,
        )
    }

    fn timestamptz_constant(value: i64) -> TypedExpr {
        TypedExpr::new(
            TypedExprKind::Constant(Value::Timestamp(value)),
            DataType::TimestampTz,
        )
    }

    fn projection(
        output_name: &str,
        expr: TypedExpr,
    ) -> crate::sql::analyzer::types::AnalyzedProjection {
        crate::sql::analyzer::types::AnalyzedProjection {
            expr,
            output_name: output_name.to_owned(),
        }
    }

    fn parse_expr(sql: &str) -> sqlparser::ast::Expr {
        let dialect = PostgreSqlDialect {};
        let stmts = Parser::parse_sql(&dialect, &format!("SELECT {sql}")).unwrap();
        match &stmts[0] {
            sqlparser::ast::Statement::Query(q) => match &*q.body {
                sqlparser::ast::SetExpr::Select(s) => match &s.projection[0] {
                    sqlparser::ast::SelectItem::UnnamedExpr(expr) => expr.clone(),
                    sqlparser::ast::SelectItem::ExprWithAlias { expr, .. } => expr.clone(),
                    other => panic!("unexpected select item: {other:?}"),
                },
                other => panic!("unexpected set expr: {other:?}"),
            },
            other => panic!("unexpected statement: {other:?}"),
        }
    }

    fn parse_query(sql: &str) -> sqlparser::ast::Query {
        let dialect = PostgreSqlDialect {};
        let stmts = Parser::parse_sql(&dialect, sql).unwrap();
        match stmts.into_iter().next().unwrap() {
            sqlparser::ast::Statement::Query(q) => *q,
            _ => panic!("expected query"),
        }
    }

    fn operator_scope() -> Scope {
        let mut scope = Scope::new();
        scope.add_table(
            "pushdown_operator_rows",
            &[
                ("id".to_string(), DataType::Int32, None),
                ("n".to_string(), DataType::Int32, None),
                ("n2".to_string(), DataType::Int64, None),
                ("like_txt".to_string(), DataType::Text, None),
                ("ilike_txt".to_string(), DataType::Text, None),
                ("flag".to_string(), DataType::Boolean, None),
                ("maybe_flag".to_string(), DataType::Boolean, None),
                ("marker".to_string(), DataType::Text, None),
            ],
        );
        scope
    }

    fn analyze_operator_expr(sql: &str) -> TypedExpr {
        let catalog = MockCatalog::builder()
            .table(
                "pushdown_operator_rows",
                vec![
                    ("id", DataType::Int32, false),
                    ("n", DataType::Int32, false),
                    ("n2", DataType::Int64, false),
                    ("like_txt", DataType::Text, false),
                    ("ilike_txt", DataType::Text, false),
                    ("flag", DataType::Boolean, false),
                    ("maybe_flag", DataType::Boolean, true),
                    ("marker", DataType::Text, true),
                ],
            )
            .build();
        Analyzer::analyze_expr_with_scope(&catalog, operator_scope(), &parse_expr(sql))
            .unwrap_or_else(|err| panic!("failed to analyze {sql:?}: {err:?}"))
    }

    fn operator_math_scope() -> Scope {
        let mut scope = Scope::new();
        scope.add_table(
            "db9_cop_operator_math_smoke",
            &[
                ("id".to_string(), DataType::Int32, None),
                ("n".to_string(), DataType::Int32, None),
                ("like_txt".to_string(), DataType::Text, None),
                ("ilike_txt".to_string(), DataType::Text, None),
                ("maybe_txt".to_string(), DataType::Text, None),
                ("fallback_txt".to_string(), DataType::Text, None),
                ("maybe_num".to_string(), DataType::Int32, None),
                ("cmp_big".to_string(), DataType::Int64, None),
                ("neg_big".to_string(), DataType::Int64, None),
                ("neg_int".to_string(), DataType::Int32, None),
                ("f8".to_string(), DataType::Float64, None),
                ("f8_low".to_string(), DataType::Float64, None),
                ("f8_high".to_string(), DataType::Float64, None),
            ],
        );
        scope
    }

    fn analyze_operator_math_expr(sql: &str) -> TypedExpr {
        let catalog = MockCatalog::builder()
            .table(
                "db9_cop_operator_math_smoke",
                vec![
                    ("id", DataType::Int32, false),
                    ("n", DataType::Int32, false),
                    ("like_txt", DataType::Text, false),
                    ("ilike_txt", DataType::Text, false),
                    ("maybe_txt", DataType::Text, true),
                    ("fallback_txt", DataType::Text, true),
                    ("maybe_num", DataType::Int32, true),
                    ("cmp_big", DataType::Int64, true),
                    ("neg_big", DataType::Int64, false),
                    ("neg_int", DataType::Int32, false),
                    ("f8", DataType::Float64, false),
                    ("f8_low", DataType::Float64, false),
                    ("f8_high", DataType::Float64, false),
                ],
            )
            .build();
        Analyzer::analyze_expr_with_scope(&catalog, operator_math_scope(), &parse_expr(sql))
            .unwrap_or_else(|err| panic!("failed to analyze {sql:?}: {err:?}"))
    }

    fn comparison_contract_scope() -> Scope {
        let mut scope = Scope::new();
        scope.add_table(
            "pushdown_comparison_contract_rows",
            &[
                ("id".to_string(), DataType::Int32, None),
                ("created_date".to_string(), DataType::Date, None),
                ("created_time".to_string(), DataType::Time, None),
                ("tenant_uuid".to_string(), DataType::Uuid, None),
            ],
        );
        scope
    }

    fn analyze_comparison_contract_expr(sql: &str) -> TypedExpr {
        let catalog = MockCatalog::builder()
            .table(
                "pushdown_comparison_contract_rows",
                vec![
                    ("id", DataType::Int32, false),
                    ("created_date", DataType::Date, false),
                    ("created_time", DataType::Time, false),
                    ("tenant_uuid", DataType::Uuid, false),
                ],
            )
            .build();
        Analyzer::analyze_expr_with_scope(&catalog, comparison_contract_scope(), &parse_expr(sql))
            .unwrap_or_else(|err| panic!("failed to analyze {sql:?}: {err:?}"))
    }

    fn string_regex_hash_scope() -> Scope {
        let mut scope = Scope::new();
        scope.add_table(
            "pushdown_string_regex_hash_rows",
            &[
                ("id".to_string(), DataType::Int32, None),
                ("n".to_string(), DataType::Int32, None),
                ("txt".to_string(), DataType::Text, None),
                ("txt2".to_string(), DataType::Text, None),
                ("trim_txt".to_string(), DataType::Text, None),
                ("csv_txt".to_string(), DataType::Text, None),
                ("ident_txt".to_string(), DataType::Text, None),
                ("char_code".to_string(), DataType::Int32, None),
                ("null_txt".to_string(), DataType::Text, None),
                ("hex_txt".to_string(), DataType::Text, None),
                ("bytes_val".to_string(), DataType::Bytes, None),
                ("marker".to_string(), DataType::Text, None),
            ],
        );
        scope
    }

    fn analyze_string_regex_hash_expr(sql: &str) -> TypedExpr {
        let catalog = MockCatalog::builder()
            .table(
                "pushdown_string_regex_hash_rows",
                vec![
                    ("id", DataType::Int32, false),
                    ("n", DataType::Int32, false),
                    ("txt", DataType::Text, false),
                    ("txt2", DataType::Text, false),
                    ("trim_txt", DataType::Text, false),
                    ("csv_txt", DataType::Text, false),
                    ("ident_txt", DataType::Text, false),
                    ("char_code", DataType::Int32, false),
                    ("null_txt", DataType::Text, true),
                    ("hex_txt", DataType::Text, false),
                    ("bytes_val", DataType::Bytes, false),
                    ("marker", DataType::Text, true),
                ],
            )
            .build();
        Analyzer::analyze_expr_with_scope(&catalog, string_regex_hash_scope(), &parse_expr(sql))
            .unwrap_or_else(|err| panic!("failed to analyze {sql:?}: {err:?}"))
    }

    fn collated_text_column_ref(
        collation: &str,
        resolved: crate::sql::collation::ResolvedCollation,
    ) -> TypedExpr {
        TypedExpr::new(
            TypedExprKind::Collate {
                expr: Box::new(column_ref(0, "name", DataType::Text)),
                collation: collation.to_owned(),
                resolved,
            },
            DataType::Text,
        )
    }

    fn array_scope() -> Scope {
        let mut scope = Scope::new();
        scope.add_table(
            "pushdown_array_rows",
            &[
                ("id".to_string(), DataType::Int32, None),
                ("n".to_string(), DataType::Int32, None),
                (
                    "tags".to_string(),
                    DataType::Array(Box::new(DataType::Text)),
                    None,
                ),
                (
                    "more_tags".to_string(),
                    DataType::Array(Box::new(DataType::Text)),
                    None,
                ),
                (
                    "ints".to_string(),
                    DataType::Array(Box::new(DataType::Int32)),
                    None,
                ),
                (
                    "nested_ints".to_string(),
                    DataType::Array(Box::new(DataType::Array(Box::new(DataType::Int32)))),
                    None,
                ),
                (
                    "nested_more_ints".to_string(),
                    DataType::Array(Box::new(DataType::Array(Box::new(DataType::Int32)))),
                    None,
                ),
                (
                    "json_vals".to_string(),
                    DataType::Array(Box::new(DataType::Json)),
                    None,
                ),
                ("json_val".to_string(), DataType::Json, None),
                (
                    "jsonb_vals".to_string(),
                    DataType::Array(Box::new(DataType::Jsonb)),
                    None,
                ),
                ("jsonb_val".to_string(), DataType::Jsonb, None),
                ("csv_txt".to_string(), DataType::Text, None),
                ("marker".to_string(), DataType::Text, None),
            ],
        );
        scope
    }

    fn analyze_array_expr(sql: &str) -> TypedExpr {
        let catalog = MockCatalog::builder()
            .table(
                "pushdown_array_rows",
                vec![
                    ("id", DataType::Int32, false),
                    ("n", DataType::Int32, false),
                    ("tags", DataType::Array(Box::new(DataType::Text)), false),
                    (
                        "more_tags",
                        DataType::Array(Box::new(DataType::Text)),
                        false,
                    ),
                    ("ints", DataType::Array(Box::new(DataType::Int32)), false),
                    (
                        "nested_ints",
                        DataType::Array(Box::new(DataType::Array(Box::new(DataType::Int32)))),
                        false,
                    ),
                    (
                        "nested_more_ints",
                        DataType::Array(Box::new(DataType::Array(Box::new(DataType::Int32)))),
                        false,
                    ),
                    (
                        "json_vals",
                        DataType::Array(Box::new(DataType::Json)),
                        false,
                    ),
                    ("json_val", DataType::Json, false),
                    (
                        "jsonb_vals",
                        DataType::Array(Box::new(DataType::Jsonb)),
                        false,
                    ),
                    ("jsonb_val", DataType::Jsonb, false),
                    ("csv_txt", DataType::Text, false),
                    ("marker", DataType::Text, true),
                ],
            )
            .build();
        Analyzer::analyze_expr_with_scope(&catalog, array_scope(), &parse_expr(sql))
            .unwrap_or_else(|err| panic!("failed to analyze {sql:?}: {err:?}"))
    }

    #[test]
    fn index_scan_does_not_fold_to_db9_cop_in_m0() {
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
            PhysicalNode::IndexScan { scan_type, .. } => {
                assert!(matches!(scan_type, ScanType::IndexScan { .. }))
            }
            other => panic!("expected IndexScan (no DB9 Cop fold), got {other:?}"),
        }
    }

    #[test]
    fn in_list_scan_does_not_fold_to_db9_cop_in_m0() {
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
            PhysicalNode::IndexScan { scan_type, .. } => {
                assert!(matches!(scan_type, ScanType::InListScan { .. }))
            }
            other => panic!("expected IndexScan (no DB9 Cop fold), got {other:?}"),
        }
    }

    #[test]
    fn index_range_scan_does_not_fold_to_db9_cop_in_m0() {
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
            PhysicalNode::IndexScan { scan_type, .. } => {
                assert!(matches!(scan_type, ScanType::IndexRangeScan { .. }))
            }
            other => panic!("expected IndexScan (no DB9 Cop fold), got {other:?}"),
        }
    }

    #[test]
    fn bounded_index_range_does_not_fold_to_db9_cop_in_m0() {
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
            PhysicalNode::IndexScan { scan_type, .. } => {
                assert!(matches!(scan_type, ScanType::IndexBoundedRangeScan { .. }))
            }
            other => panic!("expected IndexScan (no DB9 Cop fold), got {other:?}"),
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
            PhysicalNode::Filter { input, .. } => {
                assert!(
                    matches!(input.node, PhysicalNode::IndexScan { .. }),
                    "expected IndexScan under local Filter, got {:?}",
                    input.node
                );
            }
            other => {
                panic!("expected local Filter over IndexScan (no DB9 Cop fold), got {other:?}")
            }
        }
    }

    #[test]
    fn null_exact_lookup_filter_is_not_elided() {
        let predicate = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(column_ref(1, "email", DataType::Text)),
                op: BinaryOp::Eq,
                right: Box::new(TypedExpr::new(
                    TypedExprKind::Constant(Value::Null),
                    DataType::Text,
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
                        scan_type: ScanType::IndexScan {
                            index_id: 7,
                            index_name: "users_email_idx".to_owned(),
                            lookup_column: Some("email".to_owned()),
                            values: vec![Value::Null],
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
            PhysicalNode::Filter { input, .. } => {
                assert!(
                    matches!(input.node, PhysicalNode::IndexScan { .. }),
                    "expected IndexScan under local Filter, got {:?}",
                    input.node
                );
            }
            other => {
                panic!("expected local Filter over IndexScan (no DB9 Cop fold), got {other:?}")
            }
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
            PhysicalNode::Filter { input, .. } => {
                assert!(
                    matches!(input.node, PhysicalNode::IndexScan { .. }),
                    "expected IndexScan under local Filter, got {:?}",
                    input.node
                );
            }
            other => {
                panic!("expected local Filter over IndexScan (no DB9 Cop fold), got {other:?}")
            }
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
        match folded.node {
            PhysicalNode::Filter { input, .. } => {
                assert!(
                    matches!(input.node, PhysicalNode::IndexScan { .. }),
                    "expected IndexScan under local Filter, got {:?}",
                    input.node
                );
            }
            other => {
                panic!("expected local Filter over IndexScan (no DB9 Cop fold), got {other:?}")
            }
        }
    }

    #[test]
    fn lowercase_builtin_function_projection_stays_local() {
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
                    node: PhysicalNode::SeqScan {
                        table_name: "users".to_owned(),
                        alias: None,
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
            PhysicalNode::Project { input, .. } => {
                assert!(matches!(input.node, PhysicalNode::Db9Cop { .. }));
            }
            other => panic!("expected local Project over Db9Cop child, got {other:?}"),
        }
    }

    #[test]
    fn uppercase_builtin_function_projection_stays_local() {
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
                    node: PhysicalNode::SeqScan {
                        table_name: "users".to_owned(),
                        alias: None,
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
            PhysicalNode::Project { input, .. } => {
                assert!(matches!(input.node, PhysicalNode::Db9Cop { .. }));
            }
            other => panic!("expected local Project over Db9Cop child, got {other:?}"),
        }
    }

    #[test]
    fn abs_builtin_function_projection_folds_to_db9_cop() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "abs_n",
                    builtin_call(
                        "abs",
                        DataType::Int64,
                        vec![column_ref(1, "n", DataType::Int64)],
                    ),
                )],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "users".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![("n".to_owned(), DataType::Int64)]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("abs_n".to_owned(), DataType::Int64)]),
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
    fn abs_return_type_mismatch_stays_local() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "abs_n",
                    builtin_call(
                        "abs",
                        DataType::Float64,
                        vec![column_ref(1, "n", DataType::Int64)],
                    ),
                )],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "users".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![("n".to_owned(), DataType::Int64)]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("abs_n".to_owned(), DataType::Float64)]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        match folded.node {
            PhysicalNode::Project { input, .. } => {
                assert!(
                    matches!(input.node, PhysicalNode::Db9Cop { .. }),
                    "expected Db9Cop under local Project, got {:?}",
                    input.node
                );
            }
            other => panic!("expected local Project, got {other:?}"),
        }
    }

    #[test]
    fn coalesce_and_nullif_projection_folds_to_db9_cop() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![
                    projection(
                        "display_name",
                        TypedExpr::new(
                            TypedExprKind::Coalesce(vec![
                                column_ref(1, "nickname", DataType::Text),
                                column_ref(2, "full_name", DataType::Text),
                                text_constant("fallback"),
                            ]),
                            DataType::Text,
                        ),
                    ),
                    projection(
                        "sanitized_name",
                        TypedExpr::new(
                            TypedExprKind::NullIf(
                                Box::new(column_ref(2, "full_name", DataType::Text)),
                                Box::new(text_constant("tmp")),
                            ),
                            DataType::Text,
                        ),
                    ),
                ],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "users".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![
                        ("nickname".to_owned(), DataType::Text),
                        ("full_name".to_owned(), DataType::Text),
                    ]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![
                ("display_name".to_owned(), DataType::Text),
                ("sanitized_name".to_owned(), DataType::Text),
            ]),
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
    fn extended_string_function_projection_stays_local() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![
                    projection(
                        "name_concat",
                        builtin_call(
                            "concat",
                            DataType::Text,
                            vec![
                                column_ref(1, "name", DataType::Text),
                                text_constant("-"),
                                column_ref(2, "delta", DataType::Int64),
                            ],
                        ),
                    ),
                    projection(
                        "name_format",
                        builtin_call(
                            "format",
                            DataType::Text,
                            vec![
                                text_constant("%s:%L"),
                                column_ref(2, "delta", DataType::Int64),
                                text_constant("it's"),
                            ],
                        ),
                    ),
                    projection(
                        "name_overlay",
                        builtin_call(
                            "overlay",
                            DataType::Text,
                            vec![
                                column_ref(1, "name", DataType::Text),
                                text_constant("ZZ"),
                                int32_constant(2),
                                int32_constant(2),
                            ],
                        ),
                    ),
                    projection(
                        "name_split",
                        builtin_call(
                            "split_part",
                            DataType::Text,
                            vec![
                                text_constant("a-Bravo-c"),
                                text_constant("-"),
                                int32_constant(2),
                            ],
                        ),
                    ),
                    projection(
                        "quoted_note",
                        builtin_call(
                            "quote_nullable",
                            DataType::Text,
                            vec![column_ref(3, "note", DataType::Text)],
                        ),
                    ),
                ],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "events".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![
                        ("id".to_owned(), DataType::Int64),
                        ("name".to_owned(), DataType::Text),
                        ("delta".to_owned(), DataType::Int64),
                        ("note".to_owned(), DataType::Text),
                    ]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![
                ("name_concat".to_owned(), DataType::Text),
                ("name_format".to_owned(), DataType::Text),
                ("name_overlay".to_owned(), DataType::Text),
                ("name_split".to_owned(), DataType::Text),
                ("quoted_note".to_owned(), DataType::Text),
            ]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        match folded.node {
            PhysicalNode::Project { input, .. } => {
                assert!(matches!(input.node, PhysicalNode::Db9Cop { .. }));
            }
            other => {
                panic!("expected local Project over Db9Cop child, got {other:?}")
            }
        }
    }

    #[test]
    fn string_regex_phase2_query_projection_stays_local() {
        let catalog = MockCatalog::builder()
            .table(
                "db9_cop_string_regex_smoke",
                vec![
                    ("id", DataType::Int32, false),
                    ("n", DataType::Int32, false),
                    ("txt", DataType::Text, false),
                    ("txt2", DataType::Text, false),
                    ("csv_txt", DataType::Text, false),
                    ("ident_txt", DataType::Text, false),
                    ("char_code", DataType::Int32, false),
                    ("null_txt", DataType::Text, true),
                ],
            )
            .build();
        let mut analyzer = Analyzer::new(&catalog);
        let query = parse_query(
            "SELECT \
                concat(txt, '-', txt2, '-', n) AS concat_out, \
                concat_ws('/', txt, NULL, txt2, n) AS concat_ws_out, \
                left(txt, 5) AS left_out, \
                right(txt, 4) AS right_out, \
                repeat(txt2, 2) AS repeat_out, \
                reverse(txt2) AS reverse_out, \
                initcap(txt) AS initcap_out, \
                ascii(txt2) AS ascii_out, \
                chr(char_code) AS chr_out, \
                lpad(txt2, 4, '0') AS lpad_out, \
                rpad(txt2, 4, '0') AS rpad_out, \
                replace(txt, 'beta', 'BETA') AS replace_out, \
                translate(txt, 'ab', 'AB') AS translate_out, \
                position('b' IN txt) AS position_out, \
                split_part(csv_txt, ',', 2) AS split_part_out, \
                quote_ident(ident_txt) AS quote_ident_out, \
                quote_literal(txt2) AS quote_literal_out, \
                quote_nullable(txt2) AS quote_nullable_out, \
                overlay(txt placing txt2 from 7 for 4) AS overlay_out, \
                format('%s/%L/%I', txt2, txt2, ident_txt) AS format_out \
             FROM db9_cop_string_regex_smoke \
             WHERE n = 20 \
             LIMIT 1",
        );
        let analyzed = analyzer.analyze_query(&query).unwrap();
        let AnalyzedQueryBody::Select(select) = &analyzed.body else {
            panic!("expected select body");
        };
        let base_schema = PlanSchema::from_columns(vec![
            ("id".to_owned(), DataType::Int32),
            ("n".to_owned(), DataType::Int32),
            ("txt".to_owned(), DataType::Text),
            ("txt2".to_owned(), DataType::Text),
            ("csv_txt".to_owned(), DataType::Text),
            ("ident_txt".to_owned(), DataType::Text),
            ("char_code".to_owned(), DataType::Int32),
            ("null_txt".to_owned(), DataType::Text),
        ]);
        let output_schema = PlanSchema::from_columns(
            analyzed
                .output_schema
                .iter()
                .map(|(name, data_type, _)| (name.clone(), data_type.clone()))
                .collect(),
        );
        let plan = PhysicalPlan {
            node: PhysicalNode::Limit {
                limit: analyzed.limit.clone(),
                offset: analyzed.offset.clone(),
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::Project {
                        projections: select.projection.clone(),
                        input: Box::new(PhysicalPlan {
                            node: PhysicalNode::SeqScan {
                                table_name: "db9_cop_string_regex_smoke".to_owned(),
                                alias: None,
                            },
                            schema: base_schema,
                            cost: PhysicalCost::default(),
                        }),
                    },
                    schema: output_schema.clone(),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: output_schema,
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        match folded.node {
            PhysicalNode::Limit { input, .. } => match input.node {
                PhysicalNode::Project { input, .. } => {
                    assert!(matches!(input.node, PhysicalNode::Db9Cop { .. }));
                }
                other => panic!("expected local Project under outer Limit, got {other:?}"),
            },
            other => panic!("expected outer Limit, got {other:?}"),
        }
    }

    #[test]
    fn quote_ident_non_text_projection_stays_local() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "quoted_id",
                    builtin_call(
                        "quote_ident",
                        DataType::Text,
                        vec![TypedExpr::new(
                            TypedExprKind::Constant(Value::Int32(7)),
                            DataType::Int32,
                        )],
                    ),
                )],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "events".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![("id".to_owned(), DataType::Int64)]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("quoted_id".to_owned(), DataType::Text)]),
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
    fn concat_temporal_projection_stays_local() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "rendered",
                    builtin_call(
                        "concat",
                        DataType::Text,
                        vec![
                            column_ref(0, "created_at", DataType::Timestamp),
                            text_constant("|"),
                            column_ref(1, "created_tz", DataType::TimestampTz),
                        ],
                    ),
                )],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "events".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![
                        ("created_at".to_owned(), DataType::Timestamp),
                        ("created_tz".to_owned(), DataType::TimestampTz),
                    ]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("rendered".to_owned(), DataType::Text)]),
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
    fn format_temporal_projection_stays_local() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "rendered",
                    builtin_call(
                        "format",
                        DataType::Text,
                        vec![
                            text_constant("%s|%L|%I"),
                            column_ref(0, "created_at", DataType::Timestamp),
                            column_ref(1, "created_tz", DataType::TimestampTz),
                            column_ref(1, "created_tz", DataType::TimestampTz),
                        ],
                    ),
                )],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "events".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![
                        ("created_at".to_owned(), DataType::Timestamp),
                        ("created_tz".to_owned(), DataType::TimestampTz),
                    ]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("rendered".to_owned(), DataType::Text)]),
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
    fn date_part_timestamp_projection_folds_to_db9_cop() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "created_day",
                    builtin_call(
                        "date_part",
                        DataType::Float64,
                        vec![
                            text_constant("day"),
                            column_ref(1, "created_at", DataType::Timestamp),
                        ],
                    ),
                )],
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
            schema: PlanSchema::from_columns(vec![("created_day".to_owned(), DataType::Float64)]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        assert!(matches!(folded.node, PhysicalNode::Db9Cop { .. }));
    }

    #[test]
    fn extract_timestamp_projection_folds_with_numeric_return_type() {
        let numeric = DataType::Numeric {
            precision: None,
            scale: None,
        };
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "created_second",
                    builtin_call(
                        "extract",
                        numeric.clone(),
                        vec![
                            text_constant("second"),
                            column_ref(1, "created_at", DataType::Timestamp),
                        ],
                    ),
                )],
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
            schema: PlanSchema::from_columns(vec![("created_second".to_owned(), numeric)]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        assert!(matches!(folded.node, PhysicalNode::Db9Cop { .. }));
    }

    #[test]
    fn date_trunc_timestamp_projection_folds_to_db9_cop() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "created_hour",
                    builtin_call(
                        "date_trunc",
                        DataType::Timestamp,
                        vec![
                            text_constant("hour"),
                            column_ref(1, "created_at", DataType::Timestamp),
                        ],
                    ),
                )],
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
            schema: PlanSchema::from_columns(vec![(
                "created_hour".to_owned(),
                DataType::Timestamp,
            )]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        assert!(matches!(folded.node, PhysicalNode::Db9Cop { .. }));
    }

    #[test]
    fn date_trunc_timestamp_filter_folds_to_db9_cop() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Filter {
                predicate: TypedExpr::new(
                    TypedExprKind::BinaryOp {
                        left: Box::new(builtin_call(
                            "date_trunc",
                            DataType::Timestamp,
                            vec![
                                text_constant("day"),
                                column_ref(1, "created_at", DataType::Timestamp),
                            ],
                        )),
                        op: BinaryOp::Eq,
                        right: Box::new(TypedExpr::new(
                            TypedExprKind::Constant(Value::Timestamp(1_704_153_600_000)),
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
        };

        let folded = apply_db9_cop_folding(plan);
        assert!(matches!(folded.node, PhysicalNode::Db9Cop { .. }));
    }

    #[test]
    fn to_char_timestamp_projection_folds_to_db9_cop() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "created_label",
                    builtin_call(
                        "to_char",
                        DataType::Text,
                        vec![
                            column_ref(1, "created_at", DataType::Timestamp),
                            text_constant("YYYY-MM-DD HH24:MI:SS"),
                        ],
                    ),
                )],
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
            schema: PlanSchema::from_columns(vec![("created_label".to_owned(), DataType::Text)]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        assert!(matches!(folded.node, PhysicalNode::Db9Cop { .. }));
    }

    #[test]
    fn to_char_timestamp_projection_with_quoted_literals_folds_to_db9_cop() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "created_label",
                    builtin_call(
                        "to_char",
                        DataType::Text,
                        vec![
                            column_ref(1, "created_at", DataType::Timestamp),
                            text_constant("YYYY-MM-DD\"T\"HH24:MI:SS\"Z\""),
                        ],
                    ),
                )],
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
            schema: PlanSchema::from_columns(vec![("created_label".to_owned(), DataType::Text)]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        assert!(matches!(folded.node, PhysicalNode::Db9Cop { .. }));
    }

    #[test]
    fn to_char_timestamp_projection_with_lowercase_tokens_folds_to_db9_cop() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "created_label",
                    builtin_call(
                        "to_char",
                        DataType::Text,
                        vec![
                            column_ref(1, "created_at", DataType::Timestamp),
                            text_constant("yyyy-mm-dd hh24:mi:ss"),
                        ],
                    ),
                )],
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
            schema: PlanSchema::from_columns(vec![("created_label".to_owned(), DataType::Text)]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        assert!(matches!(folded.node, PhysicalNode::Db9Cop { .. }));
    }

    #[test]
    fn to_char_timestamp_projection_with_unterminated_literal_folds_to_db9_cop() {
        let expr = builtin_call(
            "to_char",
            DataType::Text,
            vec![
                column_ref(1, "created_at", DataType::Timestamp),
                text_constant("YYYY\"x"),
            ],
        );

        assert!(db9_cop_expr_supported(&expr));
    }

    #[test]
    fn to_char_timestamp_projection_with_bare_t_literal_stays_local() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "created_label",
                    builtin_call(
                        "to_char",
                        DataType::Text,
                        vec![
                            column_ref(1, "created_at", DataType::Timestamp),
                            text_constant("YYYY-MM-DDTHH24:MI:SS"),
                        ],
                    ),
                )],
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
            schema: PlanSchema::from_columns(vec![("created_label".to_owned(), DataType::Text)]),
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
    fn to_char_timestamp_projection_with_unsupported_token_stays_local() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "created_label",
                    builtin_call(
                        "to_char",
                        DataType::Text,
                        vec![
                            column_ref(1, "created_at", DataType::Timestamp),
                            text_constant("Month DD, YYYY"),
                        ],
                    ),
                )],
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
            schema: PlanSchema::from_columns(vec![("created_label".to_owned(), DataType::Text)]),
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
    fn to_char_timestamp_projection_with_oversized_pattern_stays_local() {
        let oversized_pattern = "YYYY".repeat(DB9_COP_MAX_TO_CHAR_PATTERN_BYTES / 4 + 1);
        let expr = builtin_call(
            "to_char",
            DataType::Text,
            vec![
                column_ref(1, "created_at", DataType::Timestamp),
                text_constant(&oversized_pattern),
            ],
        );

        assert!(!db9_cop_expr_supported(&expr));
    }

    #[test]
    fn mod_float8_projection_stays_local() {
        let expr = builtin_call(
            "mod",
            DataType::Float64,
            vec![
                column_ref(0, "left_f8", DataType::Float64),
                column_ref(1, "right_f8", DataType::Float64),
            ],
        );

        assert!(!db9_cop_expr_supported(&expr));
    }

    #[test]
    fn unsupported_date_part_field_projection_stays_local() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "tz_offset",
                    builtin_call(
                        "date_part",
                        DataType::Float64,
                        vec![
                            text_constant("timezone"),
                            column_ref(1, "created_at", DataType::Timestamp),
                        ],
                    ),
                )],
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
            schema: PlanSchema::from_columns(vec![("tz_offset".to_owned(), DataType::Float64)]),
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
    fn timestamptz_date_part_projection_stays_local() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "created_day",
                    builtin_call(
                        "date_part",
                        DataType::Float64,
                        vec![
                            text_constant("day"),
                            column_ref(1, "created_tz", DataType::TimestampTz),
                        ],
                    ),
                )],
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
            schema: PlanSchema::from_columns(vec![("created_day".to_owned(), DataType::Float64)]),
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
    fn timestamptz_date_projection_stays_local() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "created_date",
                    builtin_call(
                        "date",
                        DataType::Date,
                        vec![column_ref(1, "created_tz", DataType::TimestampTz)],
                    ),
                )],
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
            schema: PlanSchema::from_columns(vec![("created_date".to_owned(), DataType::Date)]),
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
    fn text_date_projection_stays_local() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "created_date",
                    builtin_call(
                        "date",
                        DataType::Date,
                        vec![column_ref(1, "created_text", DataType::Text)],
                    ),
                )],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "events".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![
                        ("id".to_owned(), DataType::Int64),
                        ("created_text".to_owned(), DataType::Text),
                    ]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("created_date".to_owned(), DataType::Date)]),
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
    fn string_expanders_stay_local() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![
                    projection(
                        "repeat_out",
                        builtin_call(
                            "repeat",
                            DataType::Text,
                            vec![column_ref(1, "txt", DataType::Text), int32_constant(2)],
                        ),
                    ),
                    projection(
                        "lpad_out",
                        builtin_call(
                            "lpad",
                            DataType::Text,
                            vec![
                                column_ref(1, "txt", DataType::Text),
                                int32_constant(4),
                                text_constant("0"),
                            ],
                        ),
                    ),
                    projection(
                        "rpad_out",
                        builtin_call(
                            "rpad",
                            DataType::Text,
                            vec![
                                column_ref(1, "txt", DataType::Text),
                                int32_constant(4),
                                text_constant("0"),
                            ],
                        ),
                    ),
                    projection(
                        "format_out",
                        builtin_call(
                            "format",
                            DataType::Text,
                            vec![
                                text_constant("%s:%L"),
                                column_ref(2, "delta", DataType::Int64),
                                text_constant("it"),
                            ],
                        ),
                    ),
                    projection(
                        "array_to_string_out",
                        builtin_call(
                            "array_to_string",
                            DataType::Text,
                            vec![
                                column_ref(3, "tags", DataType::Array(Box::new(DataType::Text))),
                                text_constant(","),
                                text_constant("*"),
                            ],
                        ),
                    ),
                    projection(
                        "encode_out",
                        builtin_call(
                            "encode",
                            DataType::Text,
                            vec![
                                column_ref(4, "bytes_val", DataType::Bytes),
                                text_constant("hex"),
                            ],
                        ),
                    ),
                ],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "events".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![
                        ("id".to_owned(), DataType::Int64),
                        ("txt".to_owned(), DataType::Text),
                        ("delta".to_owned(), DataType::Int64),
                        ("tags".to_owned(), DataType::Array(Box::new(DataType::Text))),
                        ("bytes_val".to_owned(), DataType::Bytes),
                    ]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![
                ("repeat_out".to_owned(), DataType::Text),
                ("lpad_out".to_owned(), DataType::Text),
                ("rpad_out".to_owned(), DataType::Text),
                ("format_out".to_owned(), DataType::Text),
                ("array_to_string_out".to_owned(), DataType::Text),
                ("encode_out".to_owned(), DataType::Text),
            ]),
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
    fn string_safe_functions_remain_pushdown_safe() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![
                    projection(
                        "left_out",
                        builtin_call(
                            "left",
                            DataType::Text,
                            vec![column_ref(1, "txt", DataType::Text), int32_constant(2)],
                        ),
                    ),
                    projection(
                        "btrim_out",
                        builtin_call(
                            "btrim",
                            DataType::Text,
                            vec![column_ref(1, "txt", DataType::Text)],
                        ),
                    ),
                    projection(
                        "right_out",
                        builtin_call(
                            "right",
                            DataType::Text,
                            vec![column_ref(1, "txt", DataType::Text), int32_constant(2)],
                        ),
                    ),
                    projection(
                        "reverse_out",
                        builtin_call(
                            "reverse",
                            DataType::Text,
                            vec![column_ref(1, "txt", DataType::Text)],
                        ),
                    ),
                    projection(
                        "ascii_out",
                        builtin_call(
                            "ascii",
                            DataType::Int32,
                            vec![column_ref(1, "txt", DataType::Text)],
                        ),
                    ),
                    projection(
                        "chr_out",
                        builtin_call(
                            "chr",
                            DataType::Text,
                            vec![column_ref(2, "char_code", DataType::Int32)],
                        ),
                    ),
                    projection(
                        "split_part_out",
                        builtin_call(
                            "split_part",
                            DataType::Text,
                            vec![
                                column_ref(1, "txt", DataType::Text),
                                text_constant(","),
                                int32_constant(2),
                            ],
                        ),
                    ),
                ],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "events".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![
                        ("id".to_owned(), DataType::Int64),
                        ("txt".to_owned(), DataType::Text),
                        ("delta".to_owned(), DataType::Int64),
                        ("char_code".to_owned(), DataType::Int32),
                    ]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![
                ("left_out".to_owned(), DataType::Text),
                ("btrim_out".to_owned(), DataType::Text),
                ("right_out".to_owned(), DataType::Text),
                ("reverse_out".to_owned(), DataType::Text),
                ("ascii_out".to_owned(), DataType::Int32),
                ("chr_out".to_owned(), DataType::Text),
                ("split_part_out".to_owned(), DataType::Text),
            ]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        match folded.node {
            PhysicalNode::Db9Cop { ops, .. } => {
                assert_eq!(ops.len(), 1);
                assert!(matches!(ops[0], Db9CopOp::Project { .. }));
            }
            other => panic!("expected Db9Cop projection for safe string functions, got {other:?}"),
        }
    }

    #[test]
    fn digest_projection_pushes_to_db9_cop() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![
                    projection(
                        "md5_note",
                        builtin_call(
                            "md5",
                            DataType::Text,
                            vec![column_ref(1, "note", DataType::Text)],
                        ),
                    ),
                    projection(
                        "sha256_payload",
                        builtin_call(
                            "sha256",
                            DataType::Bytes,
                            vec![column_ref(2, "payload", DataType::Bytes)],
                        ),
                    ),
                    projection(
                        "digest_note",
                        builtin_call(
                            "digest",
                            DataType::Bytes,
                            vec![
                                column_ref(1, "note", DataType::Text),
                                text_constant("sha256"),
                            ],
                        ),
                    ),
                ],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "events".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![
                        ("id".to_owned(), DataType::Int64),
                        ("note".to_owned(), DataType::Text),
                        ("payload".to_owned(), DataType::Bytes),
                    ]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![
                ("md5_note".to_owned(), DataType::Text),
                ("sha256_payload".to_owned(), DataType::Bytes),
                ("digest_note".to_owned(), DataType::Bytes),
            ]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        match folded.node {
            PhysicalNode::Db9Cop { ops, .. } => {
                assert_eq!(ops.len(), 1);
                assert!(matches!(ops[0], Db9CopOp::Project { .. }));
            }
            other => panic!("expected Db9Cop Project with digest pushed, got {other:?}"),
        }
    }

    #[test]
    fn analyzed_digest_projection_stays_pushdown_safe_on_orm_shape() {
        let expr = analyze_string_regex_hash_expr("digest(txt2, 'sha256')");
        assert!(
            db9_cop_expr_supported(&expr),
            "digest projection from ORM string/hash suite should stay pushdown-safe: {expr:?}"
        );
    }

    #[test]
    fn analyzed_regexp_replace_projection_stays_local_on_exact_pair() {
        let expr = analyze_string_regex_hash_expr("regexp_replace(txt2, '[ae]', 'X', 'gi')");
        assert!(
            !db9_cop_expr_supported(&expr),
            "regexp_replace must stay local until paired DB9 Cop regex semantics catch up: {expr:?}"
        );
    }

    #[test]
    fn regex_operators_stay_local_on_exact_pair() {
        let predicate = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(column_ref(1, "note", DataType::Text)),
                op: BinaryOp::RegexMatch,
                right: Box::new(text_constant(r"(.)\1")),
            },
            DataType::Boolean,
        );
        assert!(
            !db9_cop_expr_supported(&predicate),
            "regex operators stay local on the exact pair until DB9 Cop gains execution budgeting"
        );
        let analyzed_predicate = analyze_string_regex_hash_expr("txt ~ ('(.)' || chr(92) || '1')");
        assert!(
            !db9_cop_expr_supported(&analyzed_predicate),
            "regex patterns assembled from expressions must stay local until folded to a constant: {analyzed_predicate:?}"
        );
        let rust_named_capture = analyze_string_regex_hash_expr("txt ~ '(?P<x>a)'");
        assert!(
            !db9_cop_expr_supported(&rust_named_capture),
            "Rust-only named captures must stay local because PostgreSQL rejects them"
        );
        let scoped_inline_flags = analyze_string_regex_hash_expr("txt ~ '(?i:a)'");
        assert!(
            !db9_cop_expr_supported(&scoped_inline_flags),
            "Rust-only scoped inline flags must stay local because PostgreSQL rejects them"
        );
        let unicode_class_escape = analyze_string_regex_hash_expr(r"txt ~ '\p{L}'");
        assert!(
            !db9_cop_expr_supported(&unicode_class_escape),
            "Rust-only Unicode class escapes must stay local because PostgreSQL rejects them"
        );
        let pg_word_constraints = analyze_string_regex_hash_expr(r"txt ~ '[[:<:]]abc[[:>:]]'");
        assert!(
            !db9_cop_expr_supported(&pg_word_constraints),
            "PostgreSQL POSIX word constraints must stay local until CSE matches PG regex semantics"
        );
        let pg_posix_class = analyze_string_regex_hash_expr(r"txt ~ '^[[:alpha:]]$'");
        assert!(
            !db9_cop_expr_supported(&pg_posix_class),
            "PostgreSQL POSIX character classes must stay local until CSE matches PG regex semantics"
        );

        let replace = builtin_call(
            "regexp_replace",
            DataType::Text,
            vec![
                column_ref(1, "note", DataType::Text),
                text_constant(r"(.)\1"),
                text_constant("X"),
            ],
        );
        assert!(
            !db9_cop_expr_supported(&replace),
            "regexp_replace remains local even for a CSE-compatible regex pattern"
        );
    }

    #[test]
    fn regex_dynamic_patterns_stay_local() {
        let predicate = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(column_ref(1, "note", DataType::Text)),
                op: BinaryOp::RegexMatch,
                right: Box::new(column_ref(2, "pattern", DataType::Text)),
            },
            DataType::Boolean,
        );
        assert!(
            !db9_cop_expr_supported(&predicate),
            "dynamic regex patterns must stay local until the pattern can be proven CSE-compatible"
        );

        let replace = builtin_call(
            "regexp_replace",
            DataType::Text,
            vec![
                column_ref(1, "note", DataType::Text),
                column_ref(2, "pattern", DataType::Text),
                text_constant("X"),
            ],
        );
        assert!(
            !db9_cop_expr_supported(&replace),
            "regexp_replace with a dynamic pattern must stay local"
        );
    }

    #[test]
    fn analyzed_regexp_matches_projection_stays_local_as_srf_surface() {
        let expr = analyze_string_regex_hash_expr("regexp_matches(txt2, '[ae]', 'g')");
        assert!(
            !db9_cop_expr_supported(&expr),
            "set-returning regexp_matches should stay local until DB9 Cop has an SRF contract: {expr:?}"
        );
    }

    #[test]
    fn quote_literal_on_bytea_stays_local() {
        let expr = analyze_string_regex_hash_expr("quote_literal(bytes_val)");
        assert!(
            !db9_cop_expr_supported(&expr),
            "quote_literal(bytea) should stay local after bytea string rendering parity fix: {expr:?}"
        );
    }

    #[test]
    fn sha256_on_bytea_pushes_to_db9_cop() {
        let expr = analyze_string_regex_hash_expr("sha256(bytes_val)");
        assert!(
            db9_cop_expr_supported(&expr),
            "sha256(bytea) is a PG-supported overload and should stay pushdown-safe: {expr:?}"
        );
    }

    #[test]
    fn sha256_on_text_stays_local() {
        let expr = builtin_call(
            "sha256",
            DataType::Bytes,
            vec![column_ref(1, "note", DataType::Text)],
        );
        assert!(
            !db9_cop_expr_supported(&expr),
            "sha256(text) must not be pushed because PostgreSQL only exposes sha256(bytea)"
        );
    }

    #[test]
    fn icu_collated_text_comparison_stays_local() {
        let expr = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(collated_text_column_ref(
                    "de_icu",
                    crate::sql::collation::ResolvedCollation::Icu("de".to_owned()),
                )),
                op: BinaryOp::Lt,
                right: Box::new(text_constant("z")),
            },
            DataType::Boolean,
        );

        assert!(
            !db9_cop_expr_supported(&expr),
            "ICU-collated text comparison must stay local because DB9 Cop strips collation metadata: {expr:?}"
        );
    }

    #[test]
    fn binary_collated_text_comparison_also_stays_local() {
        let expr = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(collated_text_column_ref(
                    "C",
                    crate::sql::collation::ResolvedCollation::Binary,
                )),
                op: BinaryOp::Lt,
                right: Box::new(text_constant("z")),
            },
            DataType::Boolean,
        );

        assert!(
            !db9_cop_expr_supported(&expr),
            "Any explicit COLLATE wrapper must stay local until DB9 Cop can preserve collation metadata end-to-end: {expr:?}"
        );
    }

    #[test]
    fn regexp_replace_multiline_flag_projection_stays_local() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "normalized_note",
                    builtin_call(
                        "regexp_replace",
                        DataType::Text,
                        vec![
                            column_ref(1, "note", DataType::Text),
                            text_constant("^b$"),
                            text_constant("X"),
                            text_constant("m"),
                        ],
                    ),
                )],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "events".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![
                        ("id".to_owned(), DataType::Int64),
                        ("note".to_owned(), DataType::Text),
                    ]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("normalized_note".to_owned(), DataType::Text)]),
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
    fn regexp_replace_projection_with_p_flag_stays_local() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "normalized_note",
                    builtin_call(
                        "regexp_replace",
                        DataType::Text,
                        vec![
                            column_ref(1, "note", DataType::Text),
                            text_constant("."),
                            text_constant("X"),
                            text_constant("gp"),
                        ],
                    ),
                )],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "events".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![
                        ("id".to_owned(), DataType::Int64),
                        ("note".to_owned(), DataType::Text),
                    ]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("normalized_note".to_owned(), DataType::Text)]),
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
    fn regexp_replace_projection_with_w_flag_stays_local() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "normalized_note",
                    builtin_call(
                        "regexp_replace",
                        DataType::Text,
                        vec![
                            column_ref(1, "note", DataType::Text),
                            text_constant("."),
                            text_constant("X"),
                            text_constant("gw"),
                        ],
                    ),
                )],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "events".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![
                        ("id".to_owned(), DataType::Int64),
                        ("note".to_owned(), DataType::Text),
                    ]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("normalized_note".to_owned(), DataType::Text)]),
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
    fn regexp_match_projection_stays_local() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "regex_parts",
                    builtin_call(
                        "regexp_match",
                        DataType::Array(Box::new(DataType::Text)),
                        vec![
                            column_ref(1, "note", DataType::Text),
                            text_constant("(a)(b)"),
                        ],
                    ),
                )],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "events".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![
                        ("id".to_owned(), DataType::Int64),
                        ("note".to_owned(), DataType::Text),
                    ]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![(
                "regex_parts".to_owned(),
                DataType::Array(Box::new(DataType::Text)),
            )]),
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
    fn substring_regex_projection_stays_local() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "substring",
                    builtin_call(
                        "substring",
                        DataType::Text,
                        vec![column_ref(1, "note", DataType::Text), text_constant("b.*")],
                    ),
                )],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "events".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![
                        ("id".to_owned(), DataType::Int64),
                        ("note".to_owned(), DataType::Text),
                    ]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("substring".to_owned(), DataType::Text)]),
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
    fn not_ilike_escape_projection_folds_to_db9_cop() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "not_ilike_match",
                    TypedExpr::new(
                        TypedExprKind::Like {
                            expr: Box::new(column_ref(1, "note", DataType::Text)),
                            pattern: Box::new(text_constant("tmp!%%")),
                            escape: Some(Box::new(text_constant("!"))),
                            case_insensitive: true,
                            negated: true,
                        },
                        DataType::Boolean,
                    ),
                )],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "events".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![
                        ("id".to_owned(), DataType::Int64),
                        ("note".to_owned(), DataType::Text),
                    ]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![(
                "not_ilike_match".to_owned(),
                DataType::Boolean,
            )]),
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
    fn string_predicate_projection_keeps_regex_local_on_exact_pair() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![
                    projection(
                        "like_match",
                        TypedExpr::new(
                            TypedExprKind::Like {
                                expr: Box::new(column_ref(1, "note", DataType::Text)),
                                pattern: Box::new(text_constant("A!_%")),
                                escape: Some(Box::new(text_constant("!"))),
                                case_insensitive: true,
                                negated: false,
                            },
                            DataType::Boolean,
                        ),
                    ),
                    projection(
                        "regex_match",
                        TypedExpr::new(
                            TypedExprKind::BinaryOp {
                                left: Box::new(column_ref(1, "note", DataType::Text)),
                                op: BinaryOp::RegexMatch,
                                right: Box::new(text_constant("^a.+z$")),
                            },
                            DataType::Boolean,
                        ),
                    ),
                    projection(
                        "regex_not_match",
                        TypedExpr::new(
                            TypedExprKind::BinaryOp {
                                left: Box::new(column_ref(1, "note", DataType::Text)),
                                op: BinaryOp::RegexNotMatch,
                                right: Box::new(text_constant("^tmp")),
                            },
                            DataType::Boolean,
                        ),
                    ),
                ],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "events".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![
                        ("id".to_owned(), DataType::Int64),
                        ("note".to_owned(), DataType::Text),
                    ]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![
                ("like_match".to_owned(), DataType::Boolean),
                ("regex_match".to_owned(), DataType::Boolean),
                ("regex_not_match".to_owned(), DataType::Boolean),
            ]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        match folded.node {
            PhysicalNode::Project { input, .. } => {
                assert!(
                    matches!(input.node, PhysicalNode::Db9Cop { .. }),
                    "LIKE can still fold under a local Project while regex projections stay local"
                );
            }
            other => {
                panic!("expected local Project over Db9Cop child, got {other:?}")
            }
        }
    }

    #[test]
    fn analyzed_operator_math_phase2_projection_batch_folds_to_db9_cop() {
        let cases = [
            ("not_ilike_flag", "ilike_txt NOT ILIKE 'tmp%'"),
            ("like_escape_flag", "like_txt LIKE 'A!_%' ESCAPE '!'"),
            ("ilike_escape_flag", "ilike_txt ILIKE 'a!%%' ESCAPE '!'"),
            ("abs_big_out", "ABS(neg_big)"),
            ("abs_int_out", "ABS(neg_int)"),
            (
                "coalesce_txt",
                "COALESCE(maybe_txt, fallback_txt, 'ultimate')",
            ),
            ("coalesce_num", "COALESCE(maybe_num, ABS(neg_int), 99)"),
            ("nullif_txt", "NULLIF(fallback_txt, maybe_txt)"),
            ("nullif_num", "NULLIF(ABS(neg_big), cmp_big)"),
            ("ceil_out", "CEIL(f8)"),
            ("ceiling_out", "CEILING(f8)"),
            ("floor_out", "FLOOR(f8)"),
            ("round_out", "ROUND(f8)"),
            ("trunc_out", "TRUNC(f8)"),
            ("cbrt_out", "CBRT(f8)"),
            ("sqrt_out", "SQRT(f8_high)"),
            ("exp_out", "EXP(f8_low)"),
            ("ln_out", "LN(f8_high)"),
            ("log_out", "LOG(f8_high)"),
            ("log10_out", "LOG10(f8_high)"),
            ("pi_out", "PI()"),
            ("sign_out", "SIGN(neg_int)"),
            ("degrees_out", "DEGREES(f8)"),
            ("radians_out", "RADIANS(f8)"),
            ("sin_out", "SIN(f8)"),
            ("cos_out", "COS(f8)"),
            ("tan_out", "TAN(f8)"),
            ("asin_out", "ASIN(f8_low)"),
            ("acos_out", "ACOS(f8_low)"),
            ("atan_out", "ATAN(f8)"),
            ("atan2_out", "ATAN2(f8, f8_low)"),
            ("mod_out", "MOD(neg_big, cmp_big)"),
            ("mod_mixed_out", "MOD(neg_int, cmp_big)"),
            ("power_out", "POWER(f8, f8_low)"),
            ("pow_out", "POW(f8, f8_low)"),
            ("hashtext_out", "HASHTEXT(fallback_txt)"),
            ("width_bucket_out", "WIDTH_BUCKET(f8, f8_low, f8_high, n)"),
        ];

        let projections = cases
            .iter()
            .map(|(name, sql)| projection(name, analyze_operator_math_expr(sql)))
            .collect::<Vec<_>>();

        let unsupported = projections
            .iter()
            .zip(cases)
            .filter(|(projection, _)| !db9_cop_expr_supported(&projection.expr))
            .map(|(projection, (name, sql))| format!("{name}: {sql} => {:?}", projection.expr.kind))
            .collect::<Vec<_>>();
        assert!(
            unsupported.is_empty(),
            "unsupported analyzed operator/math projections: {unsupported:#?}"
        );

        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections,
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "db9_cop_operator_math_smoke".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![
                        ("id".to_owned(), DataType::Int32),
                        ("n".to_owned(), DataType::Int32),
                        ("like_txt".to_owned(), DataType::Text),
                        ("ilike_txt".to_owned(), DataType::Text),
                        ("maybe_txt".to_owned(), DataType::Text),
                        ("fallback_txt".to_owned(), DataType::Text),
                        ("maybe_num".to_owned(), DataType::Int32),
                        ("cmp_big".to_owned(), DataType::Int64),
                        ("neg_big".to_owned(), DataType::Int64),
                        ("neg_int".to_owned(), DataType::Int32),
                        ("f8".to_owned(), DataType::Float64),
                        ("f8_low".to_owned(), DataType::Float64),
                        ("f8_high".to_owned(), DataType::Float64),
                    ]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![
                ("not_ilike_flag".to_owned(), DataType::Boolean),
                ("like_escape_flag".to_owned(), DataType::Boolean),
                ("ilike_escape_flag".to_owned(), DataType::Boolean),
                ("abs_big_out".to_owned(), DataType::Int64),
                ("abs_int_out".to_owned(), DataType::Int32),
                ("coalesce_txt".to_owned(), DataType::Text),
                ("coalesce_num".to_owned(), DataType::Int32),
                ("nullif_txt".to_owned(), DataType::Text),
                ("nullif_num".to_owned(), DataType::Int64),
                ("ceil_out".to_owned(), DataType::Float64),
                ("ceiling_out".to_owned(), DataType::Float64),
                ("floor_out".to_owned(), DataType::Float64),
                ("round_out".to_owned(), DataType::Float64),
                ("trunc_out".to_owned(), DataType::Float64),
                ("cbrt_out".to_owned(), DataType::Float64),
                ("sqrt_out".to_owned(), DataType::Float64),
                ("exp_out".to_owned(), DataType::Float64),
                ("ln_out".to_owned(), DataType::Float64),
                ("log_out".to_owned(), DataType::Float64),
                ("log10_out".to_owned(), DataType::Float64),
                ("pi_out".to_owned(), DataType::Float64),
                ("sign_out".to_owned(), DataType::Float64),
                ("degrees_out".to_owned(), DataType::Float64),
                ("radians_out".to_owned(), DataType::Float64),
                ("sin_out".to_owned(), DataType::Float64),
                ("cos_out".to_owned(), DataType::Float64),
                ("tan_out".to_owned(), DataType::Float64),
                ("asin_out".to_owned(), DataType::Float64),
                ("acos_out".to_owned(), DataType::Float64),
                ("atan_out".to_owned(), DataType::Float64),
                ("atan2_out".to_owned(), DataType::Float64),
                ("mod_out".to_owned(), DataType::Int64),
                ("mod_mixed_out".to_owned(), DataType::Int64),
                ("power_out".to_owned(), DataType::Float64),
                ("pow_out".to_owned(), DataType::Float64),
                ("hashtext_out".to_owned(), DataType::Int32),
                ("width_bucket_out".to_owned(), DataType::Int32),
            ]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        match folded.node {
            PhysicalNode::Db9Cop { ops, .. } => {
                assert_eq!(ops.len(), 1);
                assert!(matches!(ops[0], Db9CopOp::Project { .. }));
            }
            other => panic!("expected Db9Cop with analyzed operator/math batch, got {other:?}"),
        }
    }

    #[test]
    fn analyzed_operator_math_two_arg_log_projection_stays_local() {
        let expr = analyze_operator_math_expr("LOG(f8_low, f8_high)");
        assert!(
            !db9_cop_expr_supported(&expr),
            "two-arg LOG should remain local until exact-pair runtime support exists"
        );

        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection("log_base_out", expr)],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::IndexScan {
                        table_name: "db9_cop_operator_math_smoke".to_owned(),
                        alias: None,
                        scan_type: ScanType::IndexScan {
                            index_id: 7,
                            index_name: "db9_cop_operator_math_smoke_n_idx".to_owned(),
                            lookup_column: Some("n".to_owned()),
                            values: vec![Value::Int32(20)],
                        },
                    },
                    schema: PlanSchema::from_columns(vec![
                        ("id".to_owned(), DataType::Int32),
                        ("n".to_owned(), DataType::Int32),
                        ("like_txt".to_owned(), DataType::Text),
                        ("ilike_txt".to_owned(), DataType::Text),
                        ("maybe_txt".to_owned(), DataType::Text),
                        ("fallback_txt".to_owned(), DataType::Text),
                        ("maybe_num".to_owned(), DataType::Int32),
                        ("cmp_big".to_owned(), DataType::Int64),
                        ("neg_big".to_owned(), DataType::Int64),
                        ("neg_int".to_owned(), DataType::Int32),
                        ("f8".to_owned(), DataType::Float64),
                        ("f8_low".to_owned(), DataType::Float64),
                        ("f8_high".to_owned(), DataType::Float64),
                    ]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("log_base_out".to_owned(), DataType::Float64)]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        match folded.node {
            PhysicalNode::Project { .. } => {}
            other => panic!("expected local Project for two-arg LOG, got {other:?}"),
        }
    }

    #[test]
    fn bitwise_projection_batch_folds_to_db9_cop() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![
                    projection(
                        "bitand_out",
                        TypedExpr::new(
                            TypedExprKind::BinaryOp {
                                left: Box::new(column_ref(0, "n", DataType::Int32)),
                                op: BinaryOp::BitwiseAnd,
                                right: Box::new(int32_constant(7)),
                            },
                            DataType::Int32,
                        ),
                    ),
                    projection(
                        "shift_out",
                        TypedExpr::new(
                            TypedExprKind::BinaryOp {
                                left: Box::new(column_ref(0, "n", DataType::Int32)),
                                op: BinaryOp::ShiftLeft,
                                right: Box::new(int32_constant(1)),
                            },
                            DataType::Int32,
                        ),
                    ),
                    projection(
                        "bitnot_out",
                        TypedExpr::new(
                            TypedExprKind::UnaryOp {
                                op: UnaryOp::BitwiseNot,
                                operand: Box::new(column_ref(0, "n", DataType::Int32)),
                            },
                            DataType::Int32,
                        ),
                    ),
                ],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "pushdown_operator_rows".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![
                        ("n".to_owned(), DataType::Int32),
                        ("maybe_flag".to_owned(), DataType::Boolean),
                    ]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![
                ("bitand_out".to_owned(), DataType::Int32),
                ("shift_out".to_owned(), DataType::Int32),
                ("bitnot_out".to_owned(), DataType::Int32),
            ]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        assert!(matches!(folded.node, PhysicalNode::Db9Cop { .. }));
    }

    #[test]
    fn analyzed_operator_predicate_projection_batch_folds_to_db9_cop() {
        let cases = [
            ("is_distinct_from_out", "maybe_flag IS DISTINCT FROM TRUE"),
            (
                "is_not_distinct_from_out",
                "maybe_flag IS NOT DISTINCT FROM NULL",
            ),
            ("between_out", "n BETWEEN 10 AND 20"),
            ("not_between_out", "n NOT BETWEEN 21 AND 30"),
            ("in_list_out", "n IN (10, 20, 30)"),
            ("not_in_out", "n NOT IN (10, 30)"),
            ("is_true_out", "flag IS TRUE"),
            ("is_not_true_out", "maybe_flag IS NOT TRUE"),
            ("is_false_out", "flag IS FALSE"),
            ("is_not_false_out", "flag IS NOT FALSE"),
            ("is_unknown_out", "maybe_flag IS UNKNOWN"),
            ("is_not_unknown_out", "flag IS NOT UNKNOWN"),
        ];

        let projections = cases
            .iter()
            .map(|(name, sql)| projection(name, analyze_operator_expr(sql)))
            .collect::<Vec<_>>();

        let unsupported = projections
            .iter()
            .zip(cases)
            .filter(|(projection, _)| !db9_cop_expr_supported(&projection.expr))
            .map(|(projection, (name, sql))| format!("{name}: {sql} => {:?}", projection.expr.kind))
            .collect::<Vec<_>>();
        assert!(
            unsupported.is_empty(),
            "unsupported analyzed operator predicate projections: {unsupported:#?}"
        );

        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections,
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "pushdown_operator_rows".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![
                        ("id".to_owned(), DataType::Int32),
                        ("n".to_owned(), DataType::Int32),
                        ("n2".to_owned(), DataType::Int64),
                        ("like_txt".to_owned(), DataType::Text),
                        ("ilike_txt".to_owned(), DataType::Text),
                        ("flag".to_owned(), DataType::Boolean),
                        ("maybe_flag".to_owned(), DataType::Boolean),
                        ("marker".to_owned(), DataType::Text),
                    ]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(
                cases
                    .iter()
                    .map(|(name, _)| (name.to_string(), DataType::Boolean))
                    .collect::<Vec<_>>(),
            ),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        match folded.node {
            PhysicalNode::Db9Cop { ops, .. } => {
                assert_eq!(ops.len(), 1);
                assert!(matches!(ops[0], Db9CopOp::Project { .. }));
            }
            other => {
                panic!("expected Db9Cop with analyzed operator predicate batch, got {other:?}")
            }
        }
    }

    #[test]
    fn scalar_date_time_uuid_predicates_stay_local_on_current_exact_pair() {
        for sql in [
            "created_date = DATE '2024-01-02'",
            "created_date < DATE '2024-01-03'",
            "created_time = TIME '03:04:05'",
            "created_time < TIME '03:04:06'",
            "tenant_uuid = '00000000-0000-0000-0000-000000000001'::uuid",
            "tenant_uuid IN ('00000000-0000-0000-0000-000000000001'::uuid, '00000000-0000-0000-0000-000000000002'::uuid)",
        ] {
            let expr = analyze_comparison_contract_expr(sql);
            assert!(
                !db9_cop_expr_supported(&expr),
                "current exact-pair planner contract must keep this predicate local: {sql}"
            );
        }
    }

    #[test]
    fn regexp_split_to_array_projection_stays_local_on_exact_pair() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "parts",
                    builtin_call(
                        "regexp_split_to_array",
                        DataType::Array(Box::new(DataType::Text)),
                        vec![column_ref(1, "note", DataType::Text), text_constant("\\s+")],
                    ),
                )],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "events".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![
                        ("id".to_owned(), DataType::Int64),
                        ("note".to_owned(), DataType::Text),
                    ]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![(
                "parts".to_owned(),
                DataType::Array(Box::new(DataType::Text)),
            )]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        match folded.node {
            PhysicalNode::Project { input, .. } => {
                assert!(matches!(input.node, PhysicalNode::Db9Cop { .. }));
            }
            other => {
                panic!(
                    "expected local Project over Db9Cop child for regexp_split_to_array, got {other:?}"
                )
            }
        }
    }

    #[test]
    fn make_date_projection_folds_to_db9_cop() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "created_date",
                    builtin_call(
                        "make_date",
                        DataType::Date,
                        vec![int32_constant(2024), int32_constant(1), int32_constant(2)],
                    ),
                )],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "events".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![("id".to_owned(), DataType::Int64)]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("created_date".to_owned(), DataType::Date)]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        match folded.node {
            PhysicalNode::Db9Cop { ops, .. } => {
                assert_eq!(ops.len(), 1);
                assert!(matches!(ops[0], Db9CopOp::Project { .. }));
            }
            other => panic!("expected Db9Cop with MAKE_DATE() projection, got {other:?}"),
        }
    }

    #[test]
    fn make_time_projection_folds_to_db9_cop() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "created_time",
                    builtin_call(
                        "make_time",
                        DataType::Time,
                        vec![
                            int32_constant(8),
                            int32_constant(15),
                            float64_constant(23.5),
                        ],
                    ),
                )],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "events".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![("id".to_owned(), DataType::Int64)]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("created_time".to_owned(), DataType::Time)]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        match folded.node {
            PhysicalNode::Db9Cop { ops, .. } => {
                assert_eq!(ops.len(), 1);
                assert!(matches!(ops[0], Db9CopOp::Project { .. }));
            }
            other => panic!("expected Db9Cop with MAKE_TIME() projection, got {other:?}"),
        }
    }

    #[test]
    fn make_timestamp_projection_stays_local() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "created_ts",
                    builtin_call(
                        "make_timestamp",
                        DataType::Timestamp,
                        vec![
                            int32_constant(2024),
                            int32_constant(1),
                            int32_constant(2),
                            int32_constant(8),
                            int32_constant(15),
                            float64_constant(23.5),
                        ],
                    ),
                )],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "events".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![("id".to_owned(), DataType::Int64)]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("created_ts".to_owned(), DataType::Timestamp)]),
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
    fn to_timestamp_projection_stays_local() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "created_tz",
                    builtin_call(
                        "to_timestamp",
                        DataType::TimestampTz,
                        vec![float64_constant(1_704_187_323.5)],
                    ),
                )],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "events".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![("id".to_owned(), DataType::Int64)]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![(
                "created_tz".to_owned(),
                DataType::TimestampTz,
            )]),
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
    fn age_two_arg_interval_projection_folds_to_db9_cop() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "elapsed",
                    builtin_call(
                        "age",
                        DataType::Interval,
                        vec![
                            column_ref(1, "created_at", DataType::Timestamp),
                            timestamp_constant(1_704_067_200_000),
                        ],
                    ),
                )],
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
            schema: PlanSchema::from_columns(vec![("elapsed".to_owned(), DataType::Interval)]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        match folded.node {
            PhysicalNode::Db9Cop { ops, .. } => {
                assert_eq!(ops.len(), 1);
                assert!(matches!(ops[0], Db9CopOp::Project { .. }));
            }
            other => panic!("expected Db9Cop age projection, got {other:?}"),
        }
    }

    #[test]
    fn age_text_arguments_are_not_pushdown_safe() {
        let expr = builtin_call(
            "age",
            DataType::Interval,
            vec![
                column_ref(0, "newer", DataType::Text),
                column_ref(1, "older", DataType::Text),
            ],
        );
        assert!(
            !db9_cop_expr_supported(&expr),
            "PG has no age(text, text) overload"
        );
    }

    #[test]
    fn timestamp_age_wrapped_in_date_part_stays_local() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "elapsed_seconds",
                    builtin_call(
                        "date_part",
                        DataType::Float64,
                        vec![
                            text_constant("epoch"),
                            builtin_call(
                                "age",
                                DataType::Interval,
                                vec![
                                    column_ref(1, "created_at", DataType::Timestamp),
                                    timestamp_constant(1_704_067_200_000),
                                ],
                            ),
                        ],
                    ),
                )],
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
            schema: PlanSchema::from_columns(vec![(
                "elapsed_seconds".to_owned(),
                DataType::Float64,
            )]),
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
    fn timestamptz_age_wrapped_in_date_part_stays_local() {
        for age_expr in [
            builtin_call(
                "age",
                DataType::Interval,
                vec![
                    column_ref(1, "created_tz", DataType::TimestampTz),
                    timestamptz_constant(1_704_067_200_000),
                ],
            ),
            builtin_call(
                "age",
                DataType::Interval,
                vec![
                    column_ref(1, "created_at", DataType::Timestamp),
                    timestamptz_constant(1_704_067_200_000),
                ],
            ),
            builtin_call(
                "age",
                DataType::Interval,
                vec![
                    column_ref(1, "created_tz", DataType::TimestampTz),
                    timestamp_constant(1_704_067_200_000),
                ],
            ),
        ] {
            let plan = PhysicalPlan {
                node: PhysicalNode::Project {
                    projections: vec![projection(
                        "elapsed_seconds",
                        builtin_call(
                            "date_part",
                            DataType::Float64,
                            vec![text_constant("epoch"), age_expr],
                        ),
                    )],
                    input: Box::new(PhysicalPlan {
                        node: PhysicalNode::SeqScan {
                            table_name: "events".to_owned(),
                            alias: None,
                        },
                        schema: PlanSchema::from_columns(vec![
                            ("id".to_owned(), DataType::Int64),
                            ("created_tz".to_owned(), DataType::TimestampTz),
                            ("created_at".to_owned(), DataType::Timestamp),
                        ]),
                        cost: PhysicalCost::default(),
                    }),
                },
                schema: PlanSchema::from_columns(vec![(
                    "elapsed_seconds".to_owned(),
                    DataType::Float64,
                )]),
                cost: PhysicalCost::default(),
            };

            let folded = apply_db9_cop_folding(plan);
            match folded.node {
                PhysicalNode::Project { input, .. } => {
                    assert!(matches!(input.node, PhysicalNode::Db9Cop { .. }));
                }
                other => panic!(
                    "expected local Project over Db9Cop child for timestamptz AGE(), got {other:?}"
                ),
            }
        }
    }

    #[test]
    fn make_interval_projection_stays_local() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "delta",
                    builtin_call(
                        "make_interval",
                        DataType::Interval,
                        vec![
                            int32_constant(0),
                            int32_constant(0),
                            int32_constant(0),
                            int32_constant(1),
                            int32_constant(2),
                            int32_constant(3),
                            float64_constant(5.5),
                        ],
                    ),
                )],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "events".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![("id".to_owned(), DataType::Int64)]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("delta".to_owned(), DataType::Interval)]),
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
    fn date_part_interval_projection_stays_local() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "seconds_v",
                    builtin_call(
                        "date_part",
                        DataType::Float64,
                        vec![
                            text_constant("epoch"),
                            builtin_call(
                                "make_interval",
                                DataType::Interval,
                                vec![
                                    int32_constant(0),
                                    int32_constant(0),
                                    int32_constant(0),
                                    int32_constant(2),
                                    int32_constant(3),
                                    int32_constant(4),
                                    float64_constant(5.5),
                                ],
                            ),
                        ],
                    ),
                )],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "events".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![("id".to_owned(), DataType::Int64)]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("seconds_v".to_owned(), DataType::Float64)]),
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
    fn age_single_arg_projection_stays_local() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "elapsed",
                    builtin_call(
                        "age",
                        DataType::Interval,
                        vec![column_ref(1, "created_at", DataType::Timestamp)],
                    ),
                )],
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
            schema: PlanSchema::from_columns(vec![("elapsed".to_owned(), DataType::Interval)]),
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
    fn make_interval_non_numeric_seconds_projection_stays_local() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "delta",
                    builtin_call(
                        "make_interval",
                        DataType::Interval,
                        vec![
                            int32_constant(0),
                            int32_constant(0),
                            int32_constant(0),
                            int32_constant(1),
                            int32_constant(2),
                            int32_constant(3),
                            text_constant("oops"),
                        ],
                    ),
                )],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "events".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![("id".to_owned(), DataType::Int64)]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("delta".to_owned(), DataType::Interval)]),
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
    fn json_scalar_function_projection_stays_local() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![
                    projection(
                        "json_len",
                        builtin_call(
                            "json_array_length",
                            DataType::Int32,
                            vec![column_ref(0, "payload_json", DataType::Json)],
                        ),
                    ),
                    projection(
                        "jsonb_kind",
                        builtin_call(
                            "jsonb_typeof",
                            DataType::Text,
                            vec![column_ref(1, "payload_jsonb", DataType::Jsonb)],
                        ),
                    ),
                    projection(
                        "json_text",
                        builtin_call(
                            "json_extract_path_text",
                            DataType::Text,
                            vec![
                                column_ref(0, "payload_json", DataType::Json),
                                text_constant("a"),
                                text_constant("b"),
                            ],
                        ),
                    ),
                    projection(
                        "jsonb_pretty",
                        builtin_call(
                            "jsonb_pretty",
                            DataType::Text,
                            vec![column_ref(1, "payload_jsonb", DataType::Jsonb)],
                        ),
                    ),
                    projection(
                        "jsonb_exists",
                        builtin_call(
                            "jsonb_exists",
                            DataType::Boolean,
                            vec![
                                column_ref(1, "payload_jsonb", DataType::Jsonb),
                                text_constant("a"),
                            ],
                        ),
                    ),
                ],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "events".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![
                        ("payload_json".to_owned(), DataType::Json),
                        ("payload_jsonb".to_owned(), DataType::Jsonb),
                    ]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![
                ("json_len".to_owned(), DataType::Int32),
                ("jsonb_kind".to_owned(), DataType::Text),
                ("json_text".to_owned(), DataType::Text),
                ("jsonb_pretty".to_owned(), DataType::Text),
                ("jsonb_exists".to_owned(), DataType::Boolean),
            ]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        match folded.node {
            PhysicalNode::Project { input, .. } => {
                assert!(matches!(input.node, PhysicalNode::SeqScan { .. }));
            }
            other => panic!("expected local Project over SeqScan child, got {other:?}"),
        }
    }

    #[test]
    fn json_column_projection_stays_local() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "payload_jsonb",
                    column_ref(0, "payload_jsonb", DataType::Jsonb),
                )],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "events".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![(
                        "payload_jsonb".to_owned(),
                        DataType::Jsonb,
                    )]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("payload_jsonb".to_owned(), DataType::Jsonb)]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        match folded.node {
            PhysicalNode::Project { input, .. } => {
                assert!(matches!(input.node, PhysicalNode::SeqScan { .. }));
            }
            other => panic!("expected local Project over SeqScan child, got {other:?}"),
        }
    }

    #[test]
    fn jsonb_build_object_projection_stays_local() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "json_obj",
                    builtin_call(
                        "jsonb_build_object",
                        DataType::Jsonb,
                        vec![
                            text_constant("id"),
                            column_ref(0, "id", DataType::Int64),
                            text_constant("note"),
                            column_ref(1, "note", DataType::Text),
                        ],
                    ),
                )],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "events".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![
                        ("id".to_owned(), DataType::Int64),
                        ("note".to_owned(), DataType::Text),
                    ]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("json_obj".to_owned(), DataType::Jsonb)]),
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
    fn jsonb_mutation_transform_projection_stays_local() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![
                    projection(
                        "jsonb_set_out",
                        builtin_call(
                            "jsonb_set",
                            DataType::Jsonb,
                            vec![
                                column_ref(0, "payload_jsonb", DataType::Jsonb),
                                text_constant("{a}"),
                                text_constant("1"),
                            ],
                        ),
                    ),
                    projection(
                        "jsonb_insert_out",
                        builtin_call(
                            "jsonb_insert",
                            DataType::Jsonb,
                            vec![
                                column_ref(0, "payload_jsonb", DataType::Jsonb),
                                text_constant("{a,0}"),
                                text_constant("1"),
                            ],
                        ),
                    ),
                    projection(
                        "jsonb_strip_nulls_out",
                        builtin_call(
                            "jsonb_strip_nulls",
                            DataType::Jsonb,
                            vec![column_ref(0, "payload_jsonb", DataType::Jsonb)],
                        ),
                    ),
                ],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "events".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![(
                        "payload_jsonb".to_owned(),
                        DataType::Jsonb,
                    )]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![
                ("jsonb_set_out".to_owned(), DataType::Jsonb),
                ("jsonb_insert_out".to_owned(), DataType::Jsonb),
                ("jsonb_strip_nulls_out".to_owned(), DataType::Jsonb),
            ]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        match folded.node {
            PhysicalNode::Project { input, .. } => {
                assert!(matches!(input.node, PhysicalNode::SeqScan { .. }));
            }
            other => panic!("expected local Project over SeqScan child, got {other:?}"),
        }
    }

    #[test]
    fn json_exists_operator_projection_stays_local() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "has_a",
                    TypedExpr::new(
                        TypedExprKind::BinaryOp {
                            left: Box::new(column_ref(0, "payload_jsonb", DataType::Jsonb)),
                            op: BinaryOp::JsonExists,
                            right: Box::new(text_constant("a")),
                        },
                        DataType::Boolean,
                    ),
                )],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "events".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![(
                        "payload_jsonb".to_owned(),
                        DataType::Jsonb,
                    )]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("has_a".to_owned(), DataType::Boolean)]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        match folded.node {
            PhysicalNode::Project { input, .. } => {
                assert!(matches!(input.node, PhysicalNode::SeqScan { .. }));
            }
            other => panic!("expected local Project over SeqScan child, got {other:?}"),
        }
    }

    #[test]
    fn json_contains_projection_stays_local() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "contains",
                    TypedExpr::new(
                        TypedExprKind::BinaryOp {
                            left: Box::new(column_ref(0, "payload_left", DataType::Jsonb)),
                            op: BinaryOp::JsonContains,
                            right: Box::new(column_ref(1, "payload_right", DataType::Jsonb)),
                        },
                        DataType::Boolean,
                    ),
                )],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "events".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![
                        ("payload_left".to_owned(), DataType::Jsonb),
                        ("payload_right".to_owned(), DataType::Jsonb),
                    ]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("contains".to_owned(), DataType::Boolean)]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        match folded.node {
            PhysicalNode::Project { input, .. } => {
                assert!(matches!(input.node, PhysicalNode::SeqScan { .. }));
            }
            other => panic!("expected local Project over SeqScan child, got {other:?}"),
        }
    }

    #[test]
    fn array_column_projection_folds_to_db9_cop() {
        let tags_type = DataType::Array(Box::new(DataType::Text));
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection("tags", column_ref(1, "tags", tags_type.clone()))],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "items".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![
                        ("id".to_owned(), DataType::Int64),
                        ("tags".to_owned(), tags_type.clone()),
                    ]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("tags".to_owned(), tags_type.clone())]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        match folded.node {
            PhysicalNode::Db9Cop { ops, .. } => {
                assert_eq!(ops.len(), 1);
                assert!(matches!(ops[0], Db9CopOp::Project { .. }));
            }
            other => panic!("expected Db9Cop with array projection, got {other:?}"),
        }
    }

    #[test]
    fn array_length_projection_folds_to_db9_cop() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "len",
                    builtin_call(
                        "array_length",
                        DataType::Int32,
                        vec![
                            TypedExpr::new(
                                TypedExprKind::ArrayLiteral(vec![
                                    TypedExpr::new(
                                        TypedExprKind::Constant(Value::Int32(1)),
                                        DataType::Int32,
                                    ),
                                    TypedExpr::new(
                                        TypedExprKind::Constant(Value::Int32(2)),
                                        DataType::Int32,
                                    ),
                                    TypedExpr::new(
                                        TypedExprKind::Constant(Value::Int32(3)),
                                        DataType::Int32,
                                    ),
                                ]),
                                DataType::Array(Box::new(DataType::Int32)),
                            ),
                            int32_constant(1),
                        ],
                    ),
                )],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "items".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![("id".to_owned(), DataType::Int64)]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("len".to_owned(), DataType::Int32)]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        match folded.node {
            PhysicalNode::Db9Cop { ops, .. } => {
                assert_eq!(ops.len(), 1);
                assert!(matches!(ops[0], Db9CopOp::Project { .. }));
            }
            other => panic!("expected Db9Cop with array_length projection, got {other:?}"),
        }
    }

    #[test]
    fn array_literal_constant_support_matches_db9_wire_encoder_contract() {
        assert!(!db9_cop_array_literal_constant_supported(
            &Value::Null,
            &DataType::Numeric {
                precision: None,
                scale: None,
            },
        ));
        assert!(db9_cop_array_literal_constant_supported(
            &Value::Text("cop".into()),
            &DataType::Varchar(16),
        ));
        assert!(db9_cop_array_literal_constant_supported(
            &Value::Timestamp(1_700_000_000_000),
            &DataType::TimestampTz,
        ));

        assert!(!db9_cop_array_literal_constant_supported(
            &Value::Date(19_724),
            &DataType::Date,
        ));
        assert!(!db9_cop_array_literal_constant_supported(
            &Value::Time(3_723_004_005),
            &DataType::Time,
        ));
        assert!(!db9_cop_array_literal_constant_supported(
            &Value::Interval(crate::model::IntervalValue::from_millis(1_000)),
            &DataType::Interval,
        ));
        assert!(!db9_cop_array_literal_constant_supported(
            &Value::Uuid([0x44; 16]),
            &DataType::Uuid,
        ));
        assert!(!db9_cop_array_literal_constant_supported(
            &Value::Json("{\"a\":1}".into()),
            &DataType::Json,
        ));
        assert!(!db9_cop_array_literal_constant_supported(
            &Value::Jsonb("{\"a\":1}".into()),
            &DataType::Jsonb,
        ));
        assert!(!db9_cop_array_literal_constant_supported(
            &Value::Numeric(rust_decimal::Decimal::new(55, 1)),
            &DataType::Numeric {
                precision: None,
                scale: None,
            },
        ));
    }

    #[test]
    fn unknown_null_constant_stays_local() {
        let unknown_null = TypedExpr::null(DataType::Unknown);
        assert!(!db9_cop_expr_supported(&unknown_null));

        let unknown_null_is_test = TypedExpr::new(
            TypedExprKind::IsTest {
                expr: Box::new(unknown_null),
                test: IsTestKind::Null,
                negated: false,
            },
            DataType::Boolean,
        );
        assert!(!db9_cop_expr_supported(&unknown_null_is_test));

        assert!(db9_cop_expr_supported(&TypedExpr::null(DataType::Text)));
    }

    #[test]
    fn constant_support_rejects_declared_type_confusion() {
        assert!(!db9_cop_constant_supported(
            &Value::Int64(7),
            &DataType::Text,
        ));
        assert!(!db9_cop_constant_supported(
            &Value::Bytes(b"abc".to_vec()),
            &DataType::Text,
        ));

        assert!(db9_cop_constant_supported(
            &Value::Text("field".to_owned()),
            &DataType::Unknown,
        ));
        assert!(db9_cop_constant_supported(
            &Value::Timestamp(1_700_000_000_000),
            &DataType::TimestampTz,
        ));
    }

    #[test]
    fn array_length_with_nonencodable_literal_is_not_db9_cop_expr_supported() {
        let expr = builtin_call(
            "array_length",
            DataType::Int32,
            vec![
                TypedExpr::new(
                    TypedExprKind::ArrayLiteral(vec![TypedExpr::new(
                        TypedExprKind::Constant(Value::Numeric(rust_decimal::Decimal::new(55, 1))),
                        DataType::Numeric {
                            precision: None,
                            scale: None,
                        },
                    )]),
                    DataType::Array(Box::new(DataType::Numeric {
                        precision: None,
                        scale: None,
                    })),
                ),
                int32_constant(1),
            ],
        );

        assert!(!db9_cop_expr_supported(&expr));
    }

    #[test]
    fn array_dimension_and_width_bucket_count_require_int4() {
        let int_array = DataType::Array(Box::new(DataType::Int32));
        for function_name in ["array_length", "array_upper", "array_lower"] {
            let expr = builtin_call(
                function_name,
                DataType::Int32,
                vec![
                    column_ref(0, "items", int_array.clone()),
                    column_ref(1, "dim", DataType::Int64),
                ],
            );
            assert!(
                !db9_cop_expr_supported(&expr),
                "{function_name} must not push bigint dimensions"
            );
        }

        let width_bucket = builtin_call(
            "width_bucket",
            DataType::Int32,
            vec![
                column_ref(0, "operand", DataType::Float64),
                column_ref(1, "low", DataType::Float64),
                column_ref(2, "high", DataType::Float64),
                column_ref(3, "bucket_count", DataType::Int64),
            ],
        );
        assert!(
            !db9_cop_expr_supported(&width_bucket),
            "width_bucket count follows PG int4 signature"
        );

        let numeric = DataType::Numeric {
            precision: None,
            scale: None,
        };
        let numeric_width_bucket = builtin_call(
            "width_bucket",
            DataType::Int32,
            vec![
                column_ref(0, "operand", numeric.clone()),
                column_ref(1, "low", numeric.clone()),
                column_ref(2, "high", numeric.clone()),
                column_ref(3, "bucket_count", DataType::Int32),
            ],
        );
        assert!(
            db9_cop_builtin_function_supported(
                "width_bucket",
                match &numeric_width_bucket.kind {
                    TypedExprKind::FunctionCall { args, .. } => args,
                    _ => unreachable!("constructed as function call"),
                },
                &DataType::Int32,
            ),
            "width_bucket has a PG numeric overload with int4 count"
        );
        assert!(
            !db9_cop_expr_supported(&numeric_width_bucket),
            "numeric column carriers are still outside the DB9 Cop wire-safe expression surface"
        );
    }

    #[test]
    fn numeric_inputs_to_float_returning_math_stay_local() {
        let numeric = DataType::Numeric {
            precision: None,
            scale: None,
        };
        for function_name in [
            "cbrt", "degrees", "radians", "sin", "cos", "tan", "atan", "asin", "acos",
        ] {
            let expr = builtin_call(
                function_name,
                DataType::Float64,
                vec![column_ref(0, "n", numeric.clone())],
            );
            assert!(
                !db9_cop_expr_supported(&expr),
                "{function_name}(numeric) must stay local until CSE accepts the same request surface"
            );
        }

        let atan2 = builtin_call(
            "atan2",
            DataType::Float64,
            vec![
                column_ref(0, "y", numeric.clone()),
                column_ref(1, "x", DataType::Float64),
            ],
        );
        assert!(
            !db9_cop_expr_supported(&atan2),
            "mixed numeric atan2 must stay local until CSE accepts the same request surface"
        );
    }

    #[test]
    fn interval_array_outputs_stay_local() {
        let interval_array = DataType::Array(Box::new(DataType::Interval));
        assert!(
            !db9_cop_output_column_type_supported(&interval_array),
            "interval[] outputs must stay local until the interval wire carrier is stable"
        );
    }

    #[test]
    fn array_output_projection_uses_scalar_wire_safe_contract() {
        for data_type in [
            DataType::Boolean,
            DataType::Int32,
            DataType::Int64,
            DataType::Float64,
            DataType::Text,
            DataType::Bytes,
            DataType::Date,
            DataType::Time,
            DataType::Timestamp,
            DataType::TimestampTz,
            DataType::Name,
            DataType::Varchar(32),
        ] {
            let array_type = DataType::Array(Box::new(data_type.clone()));
            assert!(
                db9_cop_output_column_type_supported(&array_type),
                "{array_type:?} should share scalar output wire support"
            );
        }

        for data_type in [
            DataType::Numeric {
                precision: None,
                scale: None,
            },
            DataType::Interval,
            DataType::Uuid,
            DataType::Json,
            DataType::Jsonb,
        ] {
            let array_type = DataType::Array(Box::new(data_type.clone()));
            assert!(
                !db9_cop_output_column_type_supported(&array_type),
                "{array_type:?} must not bypass scalar output rejection"
            );
        }
    }

    #[test]
    fn array_equality_uses_scalar_equality_contract() {
        for data_type in [
            DataType::Boolean,
            DataType::Int32,
            DataType::Int64,
            DataType::Float64,
            DataType::Text,
            DataType::Bytes,
            DataType::Timestamp,
            DataType::TimestampTz,
            DataType::Name,
            DataType::Varchar(32),
        ] {
            let array_type = DataType::Array(Box::new(data_type.clone()));
            assert!(
                db9_cop_equality_operand_types_supported(&array_type, &array_type),
                "{array_type:?} should share scalar equality support"
            );
        }

        for data_type in [
            DataType::Numeric {
                precision: None,
                scale: None,
            },
            DataType::Date,
            DataType::Time,
            DataType::Uuid,
            DataType::Json,
            DataType::Jsonb,
        ] {
            let array_type = DataType::Array(Box::new(data_type.clone()));
            assert!(
                !db9_cop_equality_operand_types_supported(&array_type, &array_type),
                "{array_type:?} must not bypass scalar equality rejection"
            );
        }
    }

    #[test]
    fn analyzed_array_projection_contract_matches_current_shape_contract() {
        let push_cases = [
            "array_length(tags, 1)",
            "array_upper(tags, 1)",
            "array_lower(tags, 1)",
            "cardinality(tags)",
            "array_position(tags, 'sql')",
            "'sql' = ANY(tags)",
            "array_cat(tags, ARRAY['cop'])",
            "array_append(tags, 'cop')",
            "array_prepend('cop', tags)",
            "array_remove(tags, 'sql')",
            "string_to_array(csv_txt, ',')",
        ];
        let unsupported = push_cases
            .iter()
            .filter_map(|sql| {
                let expr = analyze_array_expr(sql);
                (!db9_cop_expr_supported(&expr)).then(|| format!("{sql} => {:?}", expr.kind))
            })
            .collect::<Vec<_>>();
        assert!(
            unsupported.is_empty(),
            "unexpected non-pushable array expressions in active ORM suite: {unsupported:#?}"
        );

        for sql in [
            "array_to_string(tags, ',', '*')",
            "array_to_string(json_vals, ',', '*')",
        ] {
            let expr = analyze_array_expr(sql);
            assert!(
                !db9_cop_expr_supported(&expr),
                "array_to_string should stay local until the output-size contract is aligned: {sql}"
            );
        }

        for sql in [
            "tags @> ARRAY['sql']",
            "ARRAY['sql'] <@ tags",
            "tags && ARRAY['sql', 'missing']",
            "array_position(json_vals, json_val)",
            "array_remove(json_vals, json_val)",
            "array_position(jsonb_vals, jsonb_val)",
            "array_remove(jsonb_vals, jsonb_val)",
            "array_cat(ints, ARRAY[[1,2],[3,4]])",
            "array_cat(ARRAY[[1,2],[3,4]], ints)",
            "array_cat(nested_ints, nested_more_ints)",
        ] {
            let expr = analyze_array_expr(sql);
            assert!(
                !db9_cop_expr_supported(&expr),
                "local-only array expression should stay local until explicitly admitted: {sql}"
            );
        }
    }

    #[test]
    fn nested_array_element_helpers_are_not_db9_cop_pushdown_safe() {
        let int_array = DataType::Array(Box::new(DataType::Int32));
        let nested_int_array = DataType::Array(Box::new(int_array.clone()));
        let nested_col = || column_ref(0, "nested_ints", nested_int_array.clone());
        let nested_more_col = || column_ref(1, "nested_more_ints", nested_int_array.clone());
        let elem_col = || column_ref(2, "nested_elem", int_array.clone());

        for (label, expr) in [
            (
                "array_position",
                builtin_call(
                    "array_position",
                    DataType::Int32,
                    vec![nested_col(), elem_col()],
                ),
            ),
            (
                "array_remove",
                builtin_call(
                    "array_remove",
                    nested_int_array.clone(),
                    vec![nested_col(), elem_col()],
                ),
            ),
            (
                "array_append",
                builtin_call(
                    "array_append",
                    nested_int_array.clone(),
                    vec![nested_col(), elem_col()],
                ),
            ),
            (
                "array_prepend",
                builtin_call(
                    "array_prepend",
                    nested_int_array.clone(),
                    vec![elem_col(), nested_col()],
                ),
            ),
            (
                "array_cat",
                builtin_call(
                    "array_cat",
                    nested_int_array.clone(),
                    vec![nested_col(), nested_more_col()],
                ),
            ),
        ] {
            assert!(
                !db9_cop_expr_supported(&expr),
                "{label} must not be pushed for top-level nested arrays"
            );
        }
    }

    #[test]
    fn string_to_array_projection_folds_to_db9_cop() {
        let return_type = DataType::Array(Box::new(DataType::Text));
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "parts",
                    builtin_call(
                        "string_to_array",
                        return_type.clone(),
                        vec![text_constant("a,b,c"), text_constant(",")],
                    ),
                )],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "items".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![("id".to_owned(), DataType::Int64)]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("parts".to_owned(), return_type.clone())]),
            cost: PhysicalCost::default(),
        };

        let folded = apply_db9_cop_folding(plan);
        match folded.node {
            PhysicalNode::Db9Cop { ops, .. } => {
                assert_eq!(ops.len(), 1);
                assert!(matches!(ops[0], Db9CopOp::Project { .. }));
            }
            other => panic!("expected Db9Cop with string_to_array projection, got {other:?}"),
        }
    }

    #[test]
    fn jsonb_constant_projection_stays_local() {
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "payload",
                    TypedExpr::new(
                        TypedExprKind::Constant(Value::Jsonb("{\"a\":1}".to_owned())),
                        DataType::Jsonb,
                    ),
                )],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "items".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![("id".to_owned(), DataType::Int64)]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("payload".to_owned(), DataType::Jsonb)]),
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
    fn array_contains_projection_stays_local() {
        let tags_type = DataType::Array(Box::new(DataType::Text));
        let plan = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![projection(
                    "has_rust",
                    TypedExpr::new(
                        TypedExprKind::BinaryOp {
                            left: Box::new(column_ref(1, "tags", tags_type.clone())),
                            op: BinaryOp::ArrayContains,
                            right: Box::new(TypedExpr::new(
                                TypedExprKind::ArrayLiteral(vec![text_constant("rust")]),
                                tags_type.clone(),
                            )),
                        },
                        DataType::Boolean,
                    ),
                )],
                input: Box::new(PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: "items".to_owned(),
                        alias: None,
                    },
                    schema: PlanSchema::from_columns(vec![
                        ("id".to_owned(), DataType::Int64),
                        ("tags".to_owned(), tags_type.clone()),
                    ]),
                    cost: PhysicalCost::default(),
                }),
            },
            schema: PlanSchema::from_columns(vec![("has_rust".to_owned(), DataType::Boolean)]),
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
                    node: PhysicalNode::SeqScan {
                        table_name: "users".to_owned(),
                        alias: None,
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
                    node: PhysicalNode::SeqScan {
                        table_name: "users".to_owned(),
                        alias: None,
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
    fn jsonb_equality_predicate_is_not_db9_cop_supported() {
        let predicate = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(column_ref(0, "payload_left", DataType::Jsonb)),
                op: BinaryOp::Eq,
                right: Box::new(column_ref(1, "payload_right", DataType::Jsonb)),
            },
            DataType::Boolean,
        );

        assert!(!db9_cop_expr_supported(&predicate));
    }

    #[test]
    fn nullif_requires_supported_equality_operands() {
        let text_nullif = TypedExpr::new(
            TypedExprKind::NullIf(
                Box::new(column_ref(0, "name", DataType::Text)),
                Box::new(text_constant("tmp")),
            ),
            DataType::Text,
        );
        assert!(db9_cop_expr_supported(&text_nullif));

        let jsonb_nullif = TypedExpr::new(
            TypedExprKind::NullIf(
                Box::new(column_ref(0, "payload_left", DataType::Jsonb)),
                Box::new(column_ref(1, "payload_right", DataType::Jsonb)),
            ),
            DataType::Jsonb,
        );
        assert!(!db9_cop_expr_supported(&jsonb_nullif));
    }

    #[test]
    fn logical_binary_ops_require_boolean_operands() {
        assert!(db9_cop_binary_op_supported(
            &BinaryOp::And,
            &DataType::Boolean,
            &DataType::Boolean,
            &DataType::Boolean,
        ));
        assert!(!db9_cop_binary_op_supported(
            &BinaryOp::And,
            &DataType::Text,
            &DataType::Text,
            &DataType::Boolean,
        ));
        assert!(!db9_cop_binary_op_supported(
            &BinaryOp::Or,
            &DataType::Jsonb,
            &DataType::Boolean,
            &DataType::Boolean,
        ));
        assert!(!db9_cop_binary_op_supported(
            &BinaryOp::And,
            &DataType::Boolean,
            &DataType::Boolean,
            &DataType::Int32,
        ));
    }

    #[test]
    fn bitwise_and_shift_admission_matches_cse_return_contract() {
        assert!(db9_cop_expr_supported(&binary_expr(
            BinaryOp::BitwiseAnd,
            DataType::Int32,
            DataType::Int32,
            DataType::Int32,
        )));
        assert!(db9_cop_expr_supported(&binary_expr(
            BinaryOp::BitwiseAnd,
            DataType::Int64,
            DataType::Int64,
            DataType::Int64,
        )));
        assert!(!db9_cop_expr_supported(&binary_expr(
            BinaryOp::BitwiseAnd,
            DataType::Int32,
            DataType::Int64,
            DataType::Int64,
        )));
        assert!(!db9_cop_expr_supported(&binary_expr(
            BinaryOp::BitwiseAnd,
            DataType::Int32,
            DataType::Int32,
            DataType::Int64,
        )));

        assert!(db9_cop_expr_supported(&binary_expr(
            BinaryOp::ShiftLeft,
            DataType::Int32,
            DataType::Int32,
            DataType::Int32,
        )));
        assert!(db9_cop_expr_supported(&binary_expr(
            BinaryOp::ShiftRight,
            DataType::Int64,
            DataType::Int32,
            DataType::Int64,
        )));
        assert!(!db9_cop_expr_supported(&binary_expr(
            BinaryOp::ShiftLeft,
            DataType::Int32,
            DataType::Int64,
            DataType::Int32,
        )));
        assert!(!db9_cop_expr_supported(&binary_expr(
            BinaryOp::ShiftRight,
            DataType::Int64,
            DataType::Int32,
            DataType::Int32,
        )));
    }

    #[test]
    fn unary_ops_require_supported_operand_types() {
        assert!(db9_cop_expr_supported(&TypedExpr::new(
            TypedExprKind::UnaryOp {
                op: UnaryOp::Not,
                operand: Box::new(column_ref(0, "flag", DataType::Boolean)),
            },
            DataType::Boolean,
        )));
        assert!(!db9_cop_expr_supported(&TypedExpr::new(
            TypedExprKind::UnaryOp {
                op: UnaryOp::Not,
                operand: Box::new(column_ref(0, "n", DataType::Int32)),
            },
            DataType::Boolean,
        )));
        assert!(db9_cop_expr_supported(&TypedExpr::new(
            TypedExprKind::UnaryOp {
                op: UnaryOp::Minus,
                operand: Box::new(column_ref(0, "n", DataType::Int64)),
            },
            DataType::Int64,
        )));
        assert!(!db9_cop_expr_supported(&TypedExpr::new(
            TypedExprKind::UnaryOp {
                op: UnaryOp::Minus,
                operand: Box::new(column_ref(0, "name", DataType::Text)),
            },
            DataType::Text,
        )));
        assert!(db9_cop_expr_supported(&TypedExpr::new(
            TypedExprKind::UnaryOp {
                op: UnaryOp::BitwiseNot,
                operand: Box::new(column_ref(0, "n", DataType::Int32)),
            },
            DataType::Int32,
        )));
        assert!(!db9_cop_expr_supported(&TypedExpr::new(
            TypedExprKind::UnaryOp {
                op: UnaryOp::BitwiseNot,
                operand: Box::new(column_ref(0, "flag", DataType::Boolean)),
            },
            DataType::Boolean,
        )));
    }

    #[test]
    fn regex_binary_ops_remain_text_only() {
        assert!(!db9_cop_binary_op_supported(
            &BinaryOp::RegexMatch,
            &DataType::Text,
            &DataType::Varchar(64),
            &DataType::Boolean,
        ));
        assert!(!db9_cop_binary_op_supported(
            &BinaryOp::RegexNotMatch,
            &DataType::Text,
            &DataType::Text,
            &DataType::Boolean,
        ));
        assert!(
            !db9_cop_binary_op_supported(
                &BinaryOp::RegexIMatch,
                &DataType::Text,
                &DataType::Text,
                &DataType::Boolean,
            ),
            "case-insensitive regex uses PostgreSQL's asymmetric simple case arcs, not Rust regex Unicode folding"
        );
        assert!(!db9_cop_binary_op_supported(
            &BinaryOp::RegexNotIMatch,
            &DataType::Text,
            &DataType::Text,
            &DataType::Boolean,
        ));
        assert!(!db9_cop_binary_op_supported(
            &BinaryOp::RegexMatch,
            &DataType::Jsonb,
            &DataType::Text,
            &DataType::Boolean,
        ));
        assert!(!db9_cop_binary_op_supported(
            &BinaryOp::RegexMatch,
            &DataType::Text,
            &DataType::Text,
            &DataType::Text,
        ));
    }

    #[test]
    fn regex_imatch_remains_db9_cop_unsupported() {
        assert!(
            !db9_cop_binary_op_supported(
                &BinaryOp::RegexIMatch,
                &DataType::Text,
                &DataType::Text,
                &DataType::Boolean,
            ),
            "RegexIMatch must stay local on this exact pair; CSE rejects __db9_regex_imatch"
        );
        assert!(
            !db9_cop_binary_op_supported(
                &BinaryOp::RegexNotIMatch,
                &DataType::Text,
                &DataType::Text,
                &DataType::Boolean,
            ),
            "RegexNotIMatch must stay local on this exact pair; CSE rejects __db9_regex_not_imatch"
        );
    }

    #[test]
    fn regex_case_insensitive_operators_inside_wrappers_stay_local() {
        for sql in [
            "COALESCE(txt ~* 'ALPHA', false)",
            "COALESCE(txt !~* 'OMEGA', true)",
            "NULLIF(txt ~* 'ALPHA', false)",
            "NULLIF(txt !~* 'OMEGA', true)",
        ] {
            let expr = analyze_string_regex_hash_expr(sql);
            assert!(
                !db9_cop_expr_supported(&expr),
                "regex operators inside wrapper expressions must stay local: {sql:?}"
            );
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
