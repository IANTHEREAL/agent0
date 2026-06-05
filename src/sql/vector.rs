use crate::sql::error::SqlError;
use anyhow::{anyhow, Result};

pub(crate) const MAX_VECTOR_DIMENSIONS: usize = 16000;

pub(crate) fn validate_vector(vec: Vec<f64>, dim: u32) -> Result<Vec<f64>> {
    validate_vector_dimensions(vec.len(), dim)?;
    for value in &vec {
        validate_vector_element(*value)?;
    }
    Ok(vec)
}

pub(crate) fn validate_vector_element(value: f64) -> Result<()> {
    if value.is_nan() {
        return Err(anyhow!("NaN not allowed in vector"));
    }
    if value.is_infinite() || value.abs() > f32::MAX as f64 {
        return Err(anyhow!("infinite value not allowed in vector"));
    }
    Ok(())
}

pub(crate) fn parse_vector_text(input: &str, dim: u32) -> Result<Vec<f64>> {
    let trimmed = input.trim();
    if !trimmed.starts_with('[') || !trimmed.ends_with(']') {
        return Err(invalid_vector_syntax(input));
    }

    let inner = &trimmed[1..trimmed.len() - 1];
    if inner.trim().is_empty() {
        return validate_vector(Vec::new(), dim);
    }

    let mut vec = Vec::new();
    for elem in inner.split(',') {
        if vec.len() == MAX_VECTOR_DIMENSIONS {
            return Err(anyhow!(
                "vector cannot have more than {} dimensions",
                MAX_VECTOR_DIMENSIONS
            ));
        }

        let token = elem.trim();
        if token.is_empty() {
            return Err(invalid_vector_syntax(input));
        }

        let value = token
            .parse::<f64>()
            .map_err(|_| invalid_vector_syntax(input))?;
        vec.push(value);
    }

    validate_vector(vec, dim)
}

fn validate_vector_dimensions(len: usize, dim: u32) -> Result<()> {
    if dim as usize > MAX_VECTOR_DIMENSIONS {
        return Err(anyhow!(
            "vector cannot have more than {} dimensions",
            MAX_VECTOR_DIMENSIONS
        ));
    }
    if len == 0 {
        return Err(anyhow!("vector must have at least 1 dimension"));
    }
    if len > MAX_VECTOR_DIMENSIONS {
        return Err(anyhow!(
            "vector cannot have more than {} dimensions",
            MAX_VECTOR_DIMENSIONS
        ));
    }
    if dim > 0 && len != dim as usize {
        return Err(anyhow!("expected {} dimensions, not {}", dim, len));
    }
    Ok(())
}

fn invalid_vector_syntax(input: &str) -> anyhow::Error {
    SqlError::InvalidInputSyntax {
        type_name: "vector".into(),
        value: input.to_string(),
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_vector_text_validates_dimensions() {
        let vec = parse_vector_text("[1, 2, 3]", 3).unwrap();
        assert_eq!(vec, vec![1.0, 2.0, 3.0]);

        let err = parse_vector_text("[1, 2]", 3).unwrap_err();
        assert!(err.to_string().contains("expected 3 dimensions, not 2"));
    }

    #[test]
    fn validate_vector_rejects_non_finite_elements() {
        let nan = validate_vector(vec![f64::NAN], 0).unwrap_err();
        assert_eq!(nan.to_string(), "NaN not allowed in vector");

        let inf = validate_vector(vec![f64::INFINITY], 0).unwrap_err();
        assert_eq!(inf.to_string(), "infinite value not allowed in vector");
    }

    #[test]
    fn validate_vector_rejects_values_outside_float4_range() {
        let err = validate_vector(vec![f32::MAX as f64 * 2.0], 0).unwrap_err();
        assert_eq!(err.to_string(), "infinite value not allowed in vector");
    }

    #[test]
    fn validate_vector_uses_pgvector_dimension_limit() {
        let err = validate_vector(vec![0.0; MAX_VECTOR_DIMENSIONS + 1], 0).unwrap_err();
        assert_eq!(
            err.to_string(),
            "vector cannot have more than 16000 dimensions"
        );
    }
}
