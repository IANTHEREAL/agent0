use super::cost_model::CostModel;
use crate::model::{DataType, IndexDef, TableSchema, Value};
use crate::sql::analyzer::types::{TypedExpr, TypedExprKind, TypedOrderByExpr};
use crate::sql::hnsw::HnswDistanceMetric;

#[derive(Debug, Clone)]
pub enum HnswQueryVector {
    Constant(Vec<Value>),
    PendingEmbedding { text: String, dimensions: u32 },
}

/// Parameters for an HNSW scan opportunity.
#[derive(Debug, Clone)]
pub struct HnswScanParams {
    pub index_id: u64,
    pub index_name: String,
    pub query_vector: HnswQueryVector,
    pub k: usize,
    pub distance_metric: HnswDistanceMetric,
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

    let distance_metric = HnswDistanceMetric::from_sql_distance_function(&func.name)?;
    let allow_text_query = HnswDistanceMetric::supports_deferred_embedding(&func.name);
    let (vector_col, vector_dimensions, query_vector) =
        extract_vector_col_and_constant_vector(&args[0], &args[1], allow_text_query)?;

    let index = indexes.iter().find(|idx| {
        idx.is_hnsw()
            && idx.columns.len() == 1
            && idx.columns[0].eq_ignore_ascii_case(vector_col)
            && idx
                .hnsw_distance_metric
                .as_deref()
                .and_then(HnswDistanceMetric::from_str)
                == Some(distance_metric)
            && idx.state == crate::worker::types::IndexState::Ready
    })?;

    let has_matching_vector_column = table_schema.columns.iter().any(|c| {
        c.name.eq_ignore_ascii_case(vector_col)
            && c.data_type == DataType::Vector(vector_dimensions)
    });
    if !has_matching_vector_column {
        return None;
    }

    Some(HnswScanParams {
        index_id: index.id,
        index_name: index.name.clone(),
        query_vector,
        k,
        distance_metric,
        distance_expr: order_expr.expr.clone(),
    })
}

fn extract_vector_col_and_constant_vector<'a>(
    left: &'a TypedExpr,
    right: &'a TypedExpr,
    allow_text_query: bool,
) -> Option<(&'a str, u32, HnswQueryVector)> {
    if let Some((col, vector_dimensions)) = try_extract_vector_column_name(left) {
        if let Some(vecv) =
            try_extract_constant_vector_or_text(right, allow_text_query, vector_dimensions)
        {
            return Some((col, vector_dimensions, vecv));
        }
    }

    if let Some((col, vector_dimensions)) = try_extract_vector_column_name(right) {
        if let Some(vecv) =
            try_extract_constant_vector_or_text(left, allow_text_query, vector_dimensions)
        {
            return Some((col, vector_dimensions, vecv));
        }
    }

    None
}

fn try_extract_vector_column_name(expr: &TypedExpr) -> Option<(&str, u32)> {
    match &expr.kind {
        TypedExprKind::ColumnRef { column_name, .. }
            if matches!(expr.data_type, DataType::Vector(_)) =>
        {
            match expr.data_type {
                DataType::Vector(dim) => Some((column_name.as_str(), dim)),
                _ => None,
            }
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
        // Handle text-literal vector syntax: '[1,0,0]'::vector produces
        // Cast(Constant(Text("[1,0,0]")), Vector(N)). We handle this in the
        // Cast arm (below) rather than as a bare Constant(Text) to avoid
        // hijacking vec_embed_* text arguments that happen to look like vectors.
        TypedExprKind::Cast {
            expr, target_type, ..
        } => {
            // When the Cast target is Vector and the inner expr is a text
            // literal, parse the text as a vector at plan time.
            if matches!(target_type, DataType::Vector(_)) {
                if let TypedExprKind::Constant(Value::Text(s)) = &expr.kind {
                    return parse_text_as_vector(s);
                }
            }
            // Otherwise recurse into the inner expression.
            try_extract_constant_vector(expr)
        }
        _ => None,
    }
}

/// Parse a text string like `"[1,0,0]"` into a vector of f64 values.
/// Returns `None` if the text is not a valid vector literal.
/// Matches runtime vector input validation but only used at plan time for
/// constant extraction.
fn parse_text_as_vector(s: &str) -> Option<Vec<Value>> {
    crate::sql::vector::parse_vector_text(s, 0)
        .ok()
        .map(|vec| vec.into_iter().map(Value::Float64).collect())
}

/// Variant of `try_extract_constant_vector` that also handles Text constants
/// for VEC_EMBED_* functions.  When the argument is a Text constant, we call
/// the EMBEDDING function to convert it to a vector at plan time (so the API
/// is called once, not per-row during HNSW scan).
fn try_extract_constant_vector_or_text(
    expr: &TypedExpr,
    allow_text_query: bool,
    target_dimensions: u32,
) -> Option<HnswQueryVector> {
    // First try the normal vector extraction path.
    if let Some(v) = try_extract_constant_vector(expr) {
        return Some(HnswQueryVector::Constant(v));
    }
    // For VEC_EMBED_* distance functions the second arg is a Text literal.
    // Defer text -> vector materialization to build time so EXPLAIN and planning
    // never trigger outbound HTTP.
    if allow_text_query {
        if let TypedExprKind::Constant(Value::Text(text)) = &expr.kind {
            return Some(HnswQueryVector::PendingEmbedding {
                text: text.clone(),
                dimensions: target_dimensions,
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ColumnDef, IndexDef};
    use crate::sql::analyzer::types::{FunctionKind, ResolvedFunction};

    fn vector_column(name: &str, dim: u32) -> ColumnDef {
        ColumnDef::new(name, DataType::Vector(dim), false)
    }

    fn hnsw_index(column: &str, metric: &str) -> IndexDef {
        IndexDef {
            name: "idx_vec".to_string(),
            id: 42,
            columns: vec![column.to_string()],
            unique: false,
            is_constraint: false,
            method: Some("hnsw".to_string()),
            predicate: None,
            expressions: vec![],
            state: crate::worker::types::IndexState::Ready,
            cached_predicate_conjuncts: None,
            hnsw_m: None,
            hnsw_ef_construction: None,
            hnsw_distance_metric: Some(metric.to_string()),
        }
    }

    fn vec_embed_order_expr(column: &str, dim: u32, text: &str) -> TypedOrderByExpr {
        TypedOrderByExpr {
            expr: TypedExpr::new(
                TypedExprKind::FunctionCall {
                    func: ResolvedFunction {
                        name: "vec_embed_l2_distance".to_string(),
                        kind: FunctionKind::Builtin,
                        return_type: DataType::Float64,
                    },
                    args: vec![
                        TypedExpr::new(
                            TypedExprKind::ColumnRef {
                                scope_depth: 0,
                                column_index: 0,
                                column_name: column.to_string(),
                            },
                            DataType::Vector(dim),
                        ),
                        TypedExpr::new(
                            TypedExprKind::Constant(Value::Text(text.to_string())),
                            DataType::Text,
                        ),
                    ],
                    order_by: vec![],
                    filter: None,
                },
                DataType::Float64,
            ),
            asc: true,
            nulls_first: false,
        }
    }

    // ── parse_text_as_vector tests ─────────────────────────────

    #[test]
    fn parse_text_vector_standard() {
        let v = parse_text_as_vector("[1,0,0]").unwrap();
        assert_eq!(
            v,
            vec![
                Value::Float64(1.0),
                Value::Float64(0.0),
                Value::Float64(0.0)
            ]
        );
    }

    #[test]
    fn parse_text_vector_with_whitespace() {
        let v = parse_text_as_vector("  [ 1 , 2 , 3 ]  ").unwrap();
        assert_eq!(
            v,
            vec![
                Value::Float64(1.0),
                Value::Float64(2.0),
                Value::Float64(3.0)
            ]
        );
    }

    #[test]
    fn parse_text_vector_negative() {
        let v = parse_text_as_vector("[-1,0.5,1]").unwrap();
        assert_eq!(
            v,
            vec![
                Value::Float64(-1.0),
                Value::Float64(0.5),
                Value::Float64(1.0)
            ]
        );
    }

    #[test]
    fn parse_text_vector_empty_brackets() {
        assert!(parse_text_as_vector("[]").is_none());
    }

    #[test]
    fn parse_text_vector_non_numeric() {
        assert!(parse_text_as_vector("[a,b,c]").is_none());
    }

    #[test]
    fn parse_text_vector_nan_rejected() {
        assert!(parse_text_as_vector("[NaN,0,0]").is_none());
    }

    #[test]
    fn parse_text_vector_inf_rejected() {
        assert!(parse_text_as_vector("[inf,0,0]").is_none());
    }

    #[test]
    fn parse_text_vector_not_brackets() {
        assert!(parse_text_as_vector("hello").is_none());
    }

    // ── try_extract_constant_vector tests ────────────────────

    #[test]
    fn extract_text_vector_through_cast() {
        // Simulates: '[1,0,0]'::vector(3) → Cast(Constant(Text("[1,0,0]")), Vector(3))
        let expr = TypedExpr::new(
            TypedExprKind::Cast {
                expr: Box::new(TypedExpr::new(
                    TypedExprKind::Constant(Value::Text("[1,0,0]".to_string())),
                    DataType::Text,
                )),
                target_type: DataType::Vector(3),
                cast_context: crate::sql::types::CastContext::Explicit,
            },
            DataType::Vector(3),
        );
        let v = try_extract_constant_vector(&expr).unwrap();
        assert_eq!(v.len(), 3);
    }

    #[test]
    fn extract_bare_text_not_treated_as_vector() {
        // Bare Constant(Text("[1,0,0]")) without Cast → should NOT match.
        // This prevents vec_embed_* text arguments from being hijacked.
        let expr = TypedExpr::new(
            TypedExprKind::Constant(Value::Text("[1,0,0]".to_string())),
            DataType::Text,
        );
        assert!(try_extract_constant_vector(&expr).is_none());
    }

    // ── detect_hnsw_scan_opportunity tests ───────────────────

    #[test]
    fn detect_hnsw_scan_carries_pending_embedding_dimensions() {
        let schema = TableSchema::new(
            "public.docs".to_string(),
            1,
            vec![vector_column("vec", 1536)],
            vec![],
        );

        let params = detect_hnsw_scan_opportunity(
            &schema,
            &[vec_embed_order_expr("vec", 1536, "hello world")],
            Some(5),
            &[hnsw_index("vec", "l2")],
        )
        .expect("HNSW opportunity should be detected");

        match params.query_vector {
            HnswQueryVector::PendingEmbedding { text, dimensions } => {
                assert_eq!(text, "hello world");
                assert_eq!(dimensions, 1536);
            }
            other => panic!("expected pending embedding, got {other:?}"),
        }
    }
}
