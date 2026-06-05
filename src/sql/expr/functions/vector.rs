use crate::model::{DataType, Value};
use crate::sql::expr::functions::embedding::{
    embed_query_text_with_cache, require_direct_embedding_superuser,
};
use crate::sql::types::cast::vector_array_elem_to_f64;
use crate::sql::vector::{
    parse_vector_text, pgvector_cosine_distance, pgvector_inner_product, pgvector_l2_distance,
    pgvector_negative_inner_product, validate_vector,
};
use anyhow::{anyhow, Result};
use std::collections::HashMap;

use super::SqlFn;
pub fn register(map: &mut HashMap<&'static str, SqlFn>) {
    map.insert("L2_DISTANCE", l2_distance_fn);
    map.insert("COSINE_DISTANCE", cosine_distance_fn);
    map.insert("INNER_PRODUCT", inner_product_fn);
    map.insert("VECTOR_NEGATIVE_INNER_PRODUCT", negative_inner_product_fn);
    map.insert("VECTOR_DIMS", vector_dims);
    map.insert("VECTOR_NORM", vector_norm_fn);
    map.insert("L2_NORMALIZE", l2_normalize_fn);
    // Auto-query: embed text at query time then compute distance.
    map.insert("VEC_EMBED_COSINE_DISTANCE", vec_embed_cosine_distance_fn);
    map.insert("VEC_EMBED_L2_DISTANCE", vec_embed_l2_distance_fn);
    map.insert("VEC_EMBED_INNER_PRODUCT", vec_embed_inner_product_fn);
    map.insert(
        "VEC_EMBED_NEGATIVE_INNER_PRODUCT",
        vec_embed_negative_inner_product_fn,
    );
}

fn extract_vector(val: &Value) -> Result<Vec<f64>> {
    match val {
        Value::Vector(vec) => validate_vector(vec.clone(), 0),
        Value::Array(arr) => {
            let vec = arr
                .iter()
                .cloned()
                .map(|elem| vector_array_elem_to_f64(elem, &DataType::Vector(0)))
                .collect::<Result<Vec<_>>>()?;
            validate_vector(vec, 0)
        }
        Value::Text(s) => parse_vector_text(s, 0),
        _ => Err(anyhow!("Expected vector, array, or text type")),
    }
}

fn l2_distance(vec1: &[f64], vec2: &[f64]) -> Result<f64> {
    if vec1.len() != vec2.len() {
        return Err(anyhow!(
            "Vectors must have same dimensions ({} vs {})",
            vec1.len(),
            vec2.len()
        ));
    }

    Ok(pgvector_l2_distance(vec1, vec2))
}

fn cosine_distance(vec1: &[f64], vec2: &[f64]) -> Result<f64> {
    if vec1.len() != vec2.len() {
        return Err(anyhow!(
            "Vectors must have same dimensions ({} vs {})",
            vec1.len(),
            vec2.len()
        ));
    }

    Ok(pgvector_cosine_distance(vec1, vec2))
}

fn inner_product(vec1: &[f64], vec2: &[f64]) -> Result<f64> {
    if vec1.len() != vec2.len() {
        return Err(anyhow!(
            "Vectors must have same dimensions ({} vs {})",
            vec1.len(),
            vec2.len()
        ));
    }

    Ok(pgvector_inner_product(vec1, vec2))
}

fn negative_inner_product(vec1: &[f64], vec2: &[f64]) -> Result<f64> {
    if vec1.len() != vec2.len() {
        return Err(anyhow!(
            "Vectors must have same dimensions ({} vs {})",
            vec1.len(),
            vec2.len()
        ));
    }

    Ok(pgvector_negative_inner_product(vec1, vec2))
}

fn vector_norm(vec: &[f64]) -> f64 {
    vec.iter().map(|x| x * x).sum::<f64>().sqrt()
}

pub fn l2_distance_fn(args: Vec<Value>) -> Result<Value> {
    if args.len() != 2 {
        return Err(anyhow!("l2_distance requires exactly 2 arguments"));
    }
    if args.iter().any(|a| matches!(a, Value::Null)) {
        return Ok(Value::Null);
    }
    let vec1 = extract_vector(&args[0])?;
    let vec2 = extract_vector(&args[1])?;
    let dist = l2_distance(&vec1, &vec2)?;
    Ok(Value::Float64(dist))
}

pub fn cosine_distance_fn(args: Vec<Value>) -> Result<Value> {
    if args.len() != 2 {
        return Err(anyhow!("cosine_distance requires exactly 2 arguments"));
    }
    if args.iter().any(|a| matches!(a, Value::Null)) {
        return Ok(Value::Null);
    }
    let vec1 = extract_vector(&args[0])?;
    let vec2 = extract_vector(&args[1])?;
    let dist = cosine_distance(&vec1, &vec2)?;
    Ok(Value::Float64(dist))
}

pub fn inner_product_fn(args: Vec<Value>) -> Result<Value> {
    if args.len() != 2 {
        return Err(anyhow!("inner_product requires exactly 2 arguments"));
    }
    if args.iter().any(|a| matches!(a, Value::Null)) {
        return Ok(Value::Null);
    }
    let vec1 = extract_vector(&args[0])?;
    let vec2 = extract_vector(&args[1])?;
    let prod = inner_product(&vec1, &vec2)?;
    Ok(Value::Float64(prod))
}

pub fn negative_inner_product_fn(args: Vec<Value>) -> Result<Value> {
    if args.len() != 2 {
        return Err(anyhow!(
            "vector_negative_inner_product requires exactly 2 arguments"
        ));
    }
    if args.iter().any(|a| matches!(a, Value::Null)) {
        return Ok(Value::Null);
    }
    let vec1 = extract_vector(&args[0])?;
    let vec2 = extract_vector(&args[1])?;
    let prod = negative_inner_product(&vec1, &vec2)?;
    Ok(Value::Float64(prod))
}

pub fn vector_dims(args: Vec<Value>) -> Result<Value> {
    if args.is_empty() {
        return Err(anyhow!("vector_dims requires 1 argument"));
    }
    if args.iter().any(|a| matches!(a, Value::Null)) {
        return Ok(Value::Null);
    }
    let vec = extract_vector(&args[0])?;
    Ok(Value::Int32(vec.len() as i32))
}

pub fn vector_norm_fn(args: Vec<Value>) -> Result<Value> {
    if args.is_empty() {
        return Err(anyhow!("vector_norm requires 1 argument"));
    }
    if args.iter().any(|a| matches!(a, Value::Null)) {
        return Ok(Value::Null);
    }
    let vec = extract_vector(&args[0])?;
    Ok(Value::Float64(vector_norm(&vec)))
}

pub fn l2_normalize_fn(args: Vec<Value>) -> Result<Value> {
    if args.is_empty() {
        return Err(anyhow!("l2_normalize requires 1 argument"));
    }
    if args.iter().any(|a| matches!(a, Value::Null)) {
        return Ok(Value::Null);
    }
    let vec = extract_vector(&args[0])?;
    let norm: f64 = vec.iter().map(|x| x * x).sum::<f64>().sqrt();
    if norm == 0.0 {
        // pgvector returns zero vector for zero input
        return Ok(Value::Vector(validate_vector(vec, 0)?));
    }
    Ok(Value::Vector(validate_vector(
        vec.iter().map(|x| x / norm).collect(),
        0,
    )?))
}

// ── Auto-query VEC_EMBED_* functions ──
//
// These functions take (VECTOR, TEXT) and auto-embed the text argument via
// the configured embedding service before computing the distance.

fn embed_text_to_vector(
    function_name: &str,
    target_dimensions: u32,
    text: &str,
) -> Result<Vec<f64>> {
    embed_query_text_with_cache(function_name, text, target_dimensions)
}

pub fn vec_embed_cosine_distance_fn(args: Vec<Value>) -> Result<Value> {
    if args.len() != 2 {
        return Err(anyhow!(
            "vec_embed_cosine_distance requires exactly 2 arguments"
        ));
    }
    if args.iter().any(|a| matches!(a, Value::Null)) {
        return Ok(Value::Null);
    }
    require_direct_embedding_superuser("vec_embed_cosine_distance")?;
    let vec1 = extract_vector(&args[0])?;
    let target_dimensions = u32::try_from(vec1.len())
        .map_err(|_| anyhow!("vector dimensions exceed supported range"))?;
    let vec2 = match &args[1] {
        Value::Text(text) => {
            embed_text_to_vector("vec_embed_cosine_distance", target_dimensions, text)?
        }
        other => extract_vector(other)?,
    };
    let dist = cosine_distance(&vec1, &vec2)?;
    Ok(Value::Float64(dist))
}

pub fn vec_embed_l2_distance_fn(args: Vec<Value>) -> Result<Value> {
    if args.len() != 2 {
        return Err(anyhow!(
            "vec_embed_l2_distance requires exactly 2 arguments"
        ));
    }
    if args.iter().any(|a| matches!(a, Value::Null)) {
        return Ok(Value::Null);
    }
    require_direct_embedding_superuser("vec_embed_l2_distance")?;
    let vec1 = extract_vector(&args[0])?;
    let target_dimensions = u32::try_from(vec1.len())
        .map_err(|_| anyhow!("vector dimensions exceed supported range"))?;
    let vec2 = match &args[1] {
        Value::Text(text) => {
            embed_text_to_vector("vec_embed_l2_distance", target_dimensions, text)?
        }
        other => extract_vector(other)?,
    };
    let dist = l2_distance(&vec1, &vec2)?;
    Ok(Value::Float64(dist))
}

pub fn vec_embed_inner_product_fn(args: Vec<Value>) -> Result<Value> {
    if args.len() != 2 {
        return Err(anyhow!(
            "vec_embed_inner_product requires exactly 2 arguments"
        ));
    }
    if args.iter().any(|a| matches!(a, Value::Null)) {
        return Ok(Value::Null);
    }
    require_direct_embedding_superuser("vec_embed_inner_product")?;
    let vec1 = extract_vector(&args[0])?;
    let target_dimensions = u32::try_from(vec1.len())
        .map_err(|_| anyhow!("vector dimensions exceed supported range"))?;
    let vec2 = match &args[1] {
        Value::Text(text) => {
            embed_text_to_vector("vec_embed_inner_product", target_dimensions, text)?
        }
        other => extract_vector(other)?,
    };
    let prod = inner_product(&vec1, &vec2)?;
    Ok(Value::Float64(prod))
}

pub fn vec_embed_negative_inner_product_fn(args: Vec<Value>) -> Result<Value> {
    if args.len() != 2 {
        return Err(anyhow!(
            "vec_embed_negative_inner_product requires exactly 2 arguments"
        ));
    }
    if args.iter().any(|a| matches!(a, Value::Null)) {
        return Ok(Value::Null);
    }
    require_direct_embedding_superuser("vec_embed_negative_inner_product")?;
    let vec1 = extract_vector(&args[0])?;
    let target_dimensions = u32::try_from(vec1.len())
        .map_err(|_| anyhow!("vector dimensions exceed supported range"))?;
    let vec2 = match &args[1] {
        Value::Text(text) => {
            embed_text_to_vector("vec_embed_negative_inner_product", target_dimensions, text)?
        }
        other => extract_vector(other)?,
    };
    let prod = negative_inner_product(&vec1, &vec2)?;
    Ok(Value::Float64(prod))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::context;
    use std::collections::HashMap;
    use std::future::Future;

    fn run_with_context<R>(is_superuser: bool, future: impl Future<Output = R>) -> R {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(context::with_context(is_superuser, "default", future))
    }

    #[test]
    fn test_l2_distance() {
        let result = l2_distance_fn(vec![
            Value::Vector(vec![0.0, 0.0]),
            Value::Vector(vec![3.0, 4.0]),
        ])
        .unwrap();
        assert_eq!(result, Value::Float64(5.0));
    }

    #[test]
    fn test_cosine_distance() {
        let result = cosine_distance_fn(vec![
            Value::Vector(vec![1.0, 0.0]),
            Value::Vector(vec![1.0, 0.0]),
        ])
        .unwrap();
        assert!(
            (result == Value::Float64(0.0))
                || matches!(result, Value::Float64(f) if f.abs() < 1e-10)
        );
    }

    #[test]
    fn test_inner_product() {
        let result = inner_product_fn(vec![
            Value::Vector(vec![1.0, 2.0]),
            Value::Vector(vec![3.0, 4.0]),
        ])
        .unwrap();
        assert_eq!(result, Value::Float64(11.0));
    }

    #[test]
    fn test_negative_inner_product() {
        let result = negative_inner_product_fn(vec![
            Value::Vector(vec![1.0, 2.0]),
            Value::Vector(vec![3.0, 4.0]),
        ])
        .unwrap();
        assert_eq!(result, Value::Float64(-11.0));
    }

    #[test]
    fn test_cosine_distance_zero_vector_returns_nan() {
        let result = cosine_distance_fn(vec![
            Value::Vector(vec![0.0, 0.0, 0.0]),
            Value::Vector(vec![1.0, 2.0, 3.0]),
        ])
        .unwrap();
        assert!(matches!(result, Value::Float64(f) if f.is_nan()));
    }

    #[test]
    fn test_decimal_distances_match_pgvector_float4_accumulation() {
        let left = Value::Vector(vec![0.4_f32 as f64, 0.5_f32 as f64, 0.6_f32 as f64]);
        let right = Value::Vector(vec![0.1_f32 as f64, 0.2_f32 as f64, 0.3_f32 as f64]);

        assert_eq!(
            l2_distance_fn(vec![left.clone(), right.clone()]).unwrap(),
            Value::Float64(0.5196152525944904)
        );
        assert_eq!(
            cosine_distance_fn(vec![left.clone(), right.clone()]).unwrap(),
            Value::Float64(0.02536811254398652)
        );
        assert_eq!(
            inner_product_fn(vec![left, right]).unwrap(),
            Value::Float64(0.320000022649765)
        );
    }

    #[test]
    fn test_vector_dims() {
        let result = vector_dims(vec![Value::Vector(vec![1.0, 2.0, 3.0])]).unwrap();
        assert_eq!(result, Value::Int32(3));
    }

    #[test]
    fn test_vector_norm() {
        let result = vector_norm_fn(vec![Value::Vector(vec![3.0, 4.0])]).unwrap();
        assert_eq!(result, Value::Float64(5.0));
    }

    #[test]
    fn test_l2_distance_null() {
        let result = l2_distance_fn(vec![Value::Null, Value::Vector(vec![1.0, 2.0, 3.0])]).unwrap();
        assert_eq!(result, Value::Null);
    }

    #[test]
    fn test_cosine_distance_null() {
        let result =
            cosine_distance_fn(vec![Value::Vector(vec![1.0, 2.0, 3.0]), Value::Null]).unwrap();
        assert_eq!(result, Value::Null);
    }

    #[test]
    fn test_vector_dims_null() {
        let result = vector_dims(vec![Value::Null]).unwrap();
        assert_eq!(result, Value::Null);
    }

    #[test]
    fn test_extract_from_text() {
        let result = vector_dims(vec![Value::Text("[1.0, 2.0, 3.0]".into())]).unwrap();
        assert_eq!(result, Value::Int32(3));
    }

    #[test]
    fn test_extract_from_array() {
        let result = vector_dims(vec![Value::Array(vec![
            Value::Float64(1.0),
            Value::Int32(2),
        ])])
        .unwrap();
        assert_eq!(result, Value::Int32(2));
    }

    #[test]
    fn test_extract_from_array_rejects_bigint_element() {
        let err = l2_distance_fn(vec![
            Value::Array(vec![Value::Int64(1)]),
            Value::Vector(vec![1.0]),
        ])
        .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("cannot cast type"));
        assert!(message.contains("BIGINT"));
        assert!(message.contains("vector(0)"));
    }

    #[test]
    fn test_l2_normalize() {
        let result = l2_normalize_fn(vec![Value::Vector(vec![3.0, 4.0])]).unwrap();
        match result {
            Value::Vector(v) => {
                assert_eq!(v, vec![0.6_f32 as f64, 0.8_f32 as f64]);
            }
            _ => panic!("Expected vector"),
        }
    }

    #[test]
    fn test_l2_normalize_null() {
        let result = l2_normalize_fn(vec![Value::Null]).unwrap();
        assert_eq!(result, Value::Null);
    }

    #[test]
    fn test_l2_normalize_zero() {
        let result = l2_normalize_fn(vec![Value::Vector(vec![0.0, 0.0])]).unwrap();
        assert_eq!(result, Value::Vector(vec![0.0, 0.0]));
    }

    #[test]
    fn test_l2_normalize_unit_vector() {
        let result = l2_normalize_fn(vec![Value::Vector(vec![1.0, 0.0, 0.0])]).unwrap();
        match result {
            Value::Vector(v) => {
                assert!((v[0] - 1.0).abs() < 1e-10);
                assert!((v[1] - 0.0).abs() < 1e-10);
                assert!((v[2] - 0.0).abs() < 1e-10);
            }
            _ => panic!("Expected vector"),
        }
    }

    #[test]
    fn register_includes_vec_embed_inner_product() {
        let mut map = HashMap::new();
        register(&mut map);
        assert!(map.contains_key("VEC_EMBED_INNER_PRODUCT"));
    }

    #[test]
    fn vec_embed_functions_short_circuit_null_before_privilege_check() {
        let result = run_with_context(false, async {
            vec_embed_cosine_distance_fn(vec![Value::Null, Value::Text("x".into())]).unwrap()
        });
        assert_eq!(result, Value::Null);

        let result = run_with_context(false, async {
            vec_embed_l2_distance_fn(vec![Value::Vector(vec![1.0]), Value::Null]).unwrap()
        });
        assert_eq!(result, Value::Null);

        let result = run_with_context(false, async {
            vec_embed_inner_product_fn(vec![Value::Null, Value::Text("x".into())]).unwrap()
        });
        assert_eq!(result, Value::Null);

        let result = run_with_context(false, async {
            vec_embed_negative_inner_product_fn(vec![Value::Null, Value::Text("x".into())]).unwrap()
        });
        assert_eq!(result, Value::Null);
    }
}
