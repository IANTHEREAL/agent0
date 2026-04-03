use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use tikv_client::Transaction;

use crate::model::{ColumnDef, DataType, UserTypeKind, Value};
use crate::sql::types::cast::coerce_value_for_column;
use crate::storage::TikvStore;

#[derive(Debug, Clone)]
pub(crate) struct EnumValueValidator {
    data_type: DataType,
    labels: HashSet<String>,
    bare_type: String,
}

impl EnumValueValidator {
    pub(crate) fn validate(&self, value: &Value) -> Result<()> {
        validate_enum_value_against_labels(&self.data_type, value, &self.labels, &self.bare_type)
    }
}

fn enum_leaf_full_name(data_type: &DataType) -> Option<&str> {
    match data_type {
        DataType::UserDefined(name) => Some(name.as_str()),
        DataType::Array(inner) => enum_leaf_full_name(inner.as_ref()),
        _ => None,
    }
}

pub(crate) async fn load_enum_value_validator(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    data_type: &DataType,
) -> Result<Option<EnumValueValidator>> {
    let Some(full_name) = enum_leaf_full_name(data_type) else {
        return Ok(None);
    };
    let Some(def) = store.get_type(txn, db_id, full_name).await? else {
        return Ok(None);
    };
    let UserTypeKind::Enum { labels } = def.kind else {
        return Ok(None);
    };

    Ok(Some(EnumValueValidator {
        data_type: data_type.clone(),
        labels: labels.into_iter().collect(),
        bare_type: def.name,
    }))
}

pub(crate) async fn coerce_and_validate_value_for_column(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    value: Value,
    col: &ColumnDef,
    enum_validator: Option<&EnumValueValidator>,
) -> Result<Value> {
    let coerced = coerce_value_for_column(value, col)?;
    if let Some(validator) = enum_validator {
        validator.validate(&coerced)?;
    } else if let Some(validator) =
        load_enum_value_validator(store, txn, db_id, &col.data_type).await?
    {
        validator.validate(&coerced)?;
    }
    Ok(coerced)
}

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

    #[test]
    fn enum_value_validator_rejects_invalid_scalar() {
        let validator = EnumValueValidator {
            data_type: DataType::UserDefined("public.mood".to_string()),
            labels: HashSet::from(["happy".to_string(), "sad".to_string()]),
            bare_type: "mood".to_string(),
        };

        let err = validator
            .validate(&Value::Text("bogus".to_string()))
            .unwrap_err()
            .to_string();

        assert!(err.contains("invalid input value for enum mood: \"bogus\""));
    }
}
