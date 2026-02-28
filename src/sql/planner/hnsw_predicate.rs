use super::cost_model::CostModel;
use crate::model::{DataType, IndexDef, TableSchema, Value};
use crate::sql::analyzer::types::{TypedExpr, TypedExprKind, TypedOrderByExpr};

/// Parameters for an HNSW scan opportunity.
#[derive(Debug, Clone)]
pub struct HnswScanParams {
    pub index_id: u64,
    pub index_name: String,
    pub query_vector: Vec<Value>,
    pub k: usize,
    pub distance_metric: String,
    pub distance_expr: TypedExpr,
}

pub fn estimate_hnsw_scan_cost(k: usize) -> f64 {
    CostModel::HNSW_SCAN_BASE_COST + k as f64 * CostModel::HNSW_PER_RESULT_COST
}

/// Detect if a query can use an HNSW index scan.
///
/// Looks for the pattern: ORDER BY distance_func(vector_col, constant_vector) LIMIT k
/// where an HNSW index exists on vector_col with matching distance metric.
pub fn detect_hnsw_scan_opportunity(
    table_schema: &TableSchema,
    order_by: &[TypedOrderByExpr],
    limit: Option<usize>,
    indexes: &[IndexDef],
) -> Option<HnswScanParams> {
    let k = limit?;
    if k == 0 || order_by.len() != 1 {
        return None;
    }

    let order_expr = &order_by[0];
    if !order_expr.asc {
        return None;
    }

    let TypedExprKind::FunctionCall { func, args, .. } = &order_expr.expr.kind else {
        return None;
    };
    if args.len() != 2 {
        return None;
    }

    let distance_metric = map_distance_metric(&func.name)?;
    let (vector_col, query_vector) = extract_vector_col_and_constant_vector(&args[0], &args[1])?;

    let index = indexes.iter().find(|idx| {
        idx.is_hnsw()
            && idx.columns.len() == 1
            && idx.columns[0].eq_ignore_ascii_case(vector_col)
            && idx.hnsw_distance_metric.as_deref() == Some(distance_metric)
            && idx.state == crate::worker::types::IndexState::Ready
    })?;

    let has_vector_column = table_schema.columns.iter().any(|c| {
        c.name.eq_ignore_ascii_case(vector_col) && matches!(c.data_type, DataType::Vector(_))
    });
    if !has_vector_column {
        return None;
    }

    Some(HnswScanParams {
        index_id: index.id,
        index_name: index.name.clone(),
        query_vector,
        k,
        distance_metric: distance_metric.to_string(),
        distance_expr: order_expr.expr.clone(),
    })
}

fn map_distance_metric(func_name: &str) -> Option<&'static str> {
    if func_name.eq_ignore_ascii_case("l2_distance") {
        Some("l2")
    } else if func_name.eq_ignore_ascii_case("cosine_distance") {
        Some("cosine")
    } else if func_name.eq_ignore_ascii_case("inner_product") {
        Some("ip")
    } else {
        None
    }
}

fn extract_vector_col_and_constant_vector<'a>(
    left: &'a TypedExpr,
    right: &'a TypedExpr,
) -> Option<(&'a str, Vec<Value>)> {
    if let (Some(col), Some(vecv)) = (
        try_extract_vector_column_name(left),
        try_extract_constant_vector(right),
    ) {
        return Some((col, vecv));
    }

    if let (Some(col), Some(vecv)) = (
        try_extract_vector_column_name(right),
        try_extract_constant_vector(left),
    ) {
        return Some((col, vecv));
    }

    None
}

fn try_extract_vector_column_name(expr: &TypedExpr) -> Option<&str> {
    match &expr.kind {
        TypedExprKind::ColumnRef { column_name, .. }
            if matches!(expr.data_type, DataType::Vector(_)) =>
        {
            Some(column_name.as_str())
        }
        TypedExprKind::Cast {
            expr,
            target_type: DataType::Vector(_),
            ..
        } => try_extract_vector_column_name(expr),
        _ => None,
    }
}

fn try_extract_constant_vector(expr: &TypedExpr) -> Option<Vec<Value>> {
    match &expr.kind {
        TypedExprKind::Constant(Value::Vector(values)) => {
            Some(values.iter().copied().map(Value::Float64).collect())
        }
        TypedExprKind::Constant(Value::Array(values)) => values
            .iter()
            .map(|v| match v {
                Value::Float64(f) => Some(Value::Float64(*f)),
                Value::Int64(i) => Some(Value::Float64(*i as f64)),
                Value::Int32(i) => Some(Value::Float64(*i as f64)),
                _ => None,
            })
            .collect(),
        TypedExprKind::Cast { expr, .. } => try_extract_constant_vector(expr),
        _ => None,
    }
}
