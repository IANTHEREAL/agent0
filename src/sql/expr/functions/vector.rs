use crate::model::Value;
use anyhow::{anyhow, Result};
use std::collections::HashMap;

use super::SqlFn;

pub fn register(map: &mut HashMap<&'static str, SqlFn>) {
    map.insert("L2_DISTANCE", l2_distance_fn);
    map.insert("COSINE_DISTANCE", cosine_distance_fn);
    map.insert("INNER_PRODUCT", inner_product_fn);
    map.insert("VECTOR_DIMS", vector_dims);
    map.insert("VECTOR_NORM", vector_norm_fn);
}

fn extract_vector(val: &Value) -> Result<Vec<f64>> {
    match val {
        Value::Vector(vec) => Ok(vec.clone()),
        Value::Array(arr) => arr
            .iter()
            .map(|v| match v {
                Value::Float64(f) => Ok(*f),
                Value::Int32(i) => Ok(*i as f64),
                Value::Int64(i) => Ok(*i as f64),
                _ => Err(anyhow!("Vector elements must be numeric")),
            })
            .collect(),
        Value::Text(s) => {
            let s = s.trim();
            if !s.starts_with('[') || !s.ends_with(']') {
                return Err(anyhow!("Invalid vector format: expected [...]"));
            }
            let inner = &s[1..s.len() - 1];
            if inner.is_empty() {
                return Ok(Vec::new());
            }
            inner
                .split(',')
                .map(|elem| {
                    elem.trim()
                        .parse::<f64>()
                        .map_err(|_| anyhow!("Invalid vector element: {}", elem))
                })
                .collect()
        }
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

    let sum: f64 = vec1
        .iter()
        .zip(vec2.iter())
        .map(|(a, b)| {
            let diff = a - b;
            diff * diff
        })
        .sum();

    Ok(sum.sqrt())
}

fn cosine_distance(vec1: &[f64], vec2: &[f64]) -> Result<f64> {
    if vec1.len() != vec2.len() {
        return Err(anyhow!(
            "Vectors must have same dimensions ({} vs {})",
            vec1.len(),
            vec2.len()
        ));
    }

    let mut dot_product = 0.0;
    let mut norm1 = 0.0;
    let mut norm2 = 0.0;

    for (a, b) in vec1.iter().zip(vec2.iter()) {
        dot_product += a * b;
        norm1 += a * a;
        norm2 += b * b;
    }

    if norm1 == 0.0 || norm2 == 0.0 {
        return Ok(1.0);
    }

    let cosine_similarity = dot_product / (norm1.sqrt() * norm2.sqrt());
    let cosine_similarity = cosine_similarity.max(-1.0).min(1.0);

    Ok(1.0 - cosine_similarity)
}

fn inner_product(vec1: &[f64], vec2: &[f64]) -> Result<f64> {
    if vec1.len() != vec2.len() {
        return Err(anyhow!(
            "Vectors must have same dimensions ({} vs {})",
            vec1.len(),
            vec2.len()
        ));
    }

    let dot: f64 = vec1.iter().zip(vec2.iter()).map(|(a, b)| a * b).sum();
    Ok(-dot)
}

fn vector_norm(vec: &[f64]) -> f64 {
    vec.iter().map(|x| x * x).sum::<f64>().sqrt()
}

pub fn l2_distance_fn(args: Vec<Value>) -> Result<Value> {
    if args.len() != 2 {
        return Err(anyhow!("l2_distance requires exactly 2 arguments"));
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
    let vec1 = extract_vector(&args[0])?;
    let vec2 = extract_vector(&args[1])?;
    let dist = cosine_distance(&vec1, &vec2)?;
    Ok(Value::Float64(dist))
}

pub fn inner_product_fn(args: Vec<Value>) -> Result<Value> {
    if args.len() != 2 {
        return Err(anyhow!("inner_product requires exactly 2 arguments"));
    }
    let vec1 = extract_vector(&args[0])?;
    let vec2 = extract_vector(&args[1])?;
    let prod = inner_product(&vec1, &vec2)?;
    Ok(Value::Float64(prod))
}

pub fn vector_dims(args: Vec<Value>) -> Result<Value> {
    if args.is_empty() {
        return Err(anyhow!("vector_dims requires 1 argument"));
    }
    let vec = extract_vector(&args[0])?;
    Ok(Value::Int32(vec.len() as i32))
}

pub fn vector_norm_fn(args: Vec<Value>) -> Result<Value> {
    if args.is_empty() {
        return Err(anyhow!("vector_norm requires 1 argument"));
    }
    let vec = extract_vector(&args[0])?;
    Ok(Value::Float64(vector_norm(&vec)))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(result, Value::Float64(-11.0));
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
    fn test_extract_from_text() {
        let result = vector_dims(vec![Value::Text("[1.0, 2.0, 3.0]".into())]).unwrap();
        assert_eq!(result, Value::Int32(3));
    }

    #[test]
    fn test_extract_from_array() {
        let result = vector_dims(vec![Value::Array(vec![
            Value::Float64(1.0),
            Value::Float64(2.0),
        ])])
        .unwrap();
        assert_eq!(result, Value::Int32(2));
    }
}
