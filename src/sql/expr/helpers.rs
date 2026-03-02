//! Shared helper functions for the expression layer.

use crate::model::Value;

/// Extract text representation from a Value (for LIKE, SIMILAR TO, regex ops).
pub(crate) fn value_to_text(v: &Value) -> String {
    match v {
        Value::Text(s) => s.clone(),
        v => v.to_string(),
    }
}
