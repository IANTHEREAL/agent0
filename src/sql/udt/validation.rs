use std::collections::HashSet;

use anyhow::{anyhow, Result};

use crate::model::{DataType, Value};

/// Validate a value assigned to an enum-typed slot using the resolved label set.
///
/// Supports scalar enums and arrays of enums. Callers are responsible for
/// providing the label set for the matching enum type.
pub(crate) fn validate_enum_value_against_labels(
    data_type: &DataType,
    value: &Value,
    labels: &HashSet<String>,
    bare_type: &str,
) -> Result<()> {
    match (data_type, value) {
        (_, Value::Null) => Ok(()),
        (DataType::UserDefined(_), Value::Text(label)) => {
            if labels.contains(label) {
                Ok(())
            } else {
                Err(anyhow!(
                    "invalid input value for enum {}: \"{}\"",
                    bare_type,
                    label
                ))
            }
        }
        (DataType::Array(inner), Value::Array(elements)) => {
            for elem in elements {
                validate_enum_value_against_labels(inner.as_ref(), elem, labels, bare_type)?;
            }
            Ok(())
        }
        (DataType::UserDefined(_), other) | (DataType::Array(_), other) => Err(anyhow!(
            "invalid input value for enum {}: {}",
            bare_type,
            other
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_enum_value_accepts_matching_scalar_label() {
        let labels = HashSet::from(["happy".to_string(), "sad".to_string()]);

        validate_enum_value_against_labels(
            &DataType::UserDefined("public.mood".to_string()),
            &Value::Text("happy".to_string()),
            &labels,
            "mood",
        )
        .unwrap();
    }

    #[test]
    fn validate_enum_value_rejects_invalid_array_label() {
        let labels = HashSet::from(["happy".to_string(), "sad".to_string()]);
        let err = validate_enum_value_against_labels(
            &DataType::Array(Box::new(DataType::UserDefined("public.mood".to_string()))),
            &Value::Array(vec![
                Value::Text("happy".to_string()),
                Value::Text("bogus".to_string()),
            ]),
            &labels,
            "mood",
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("invalid input value for enum mood: \"bogus\""));
    }
}
