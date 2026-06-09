use crate::model::{DataType, Value};
use crate::sql::error::SqlError;
use crate::sql::types::coercion::{common_type, is_implicitly_coercible};
use anyhow::{anyhow, Result};
use std::collections::HashMap;

use super::SqlFn;

const MAX_ARRAY_RECURSION_DEPTH: usize = 64;

pub fn register(map: &mut HashMap<&'static str, SqlFn>) {
    map.insert("ARRAY_LENGTH", array_length);
    map.insert("ARRAY_UPPER", array_upper);
    map.insert("ARRAY_LOWER", array_lower);
    map.insert("CARDINALITY", cardinality);
    map.insert("ARRAY_POSITION", array_position);
    map.insert("__DB9_EQ_ANY", eq_any);
    map.insert("ARRAY_CAT", array_cat);
    map.insert("ARRAY_APPEND", array_append);
    map.insert("ARRAY_PREPEND", array_prepend);
    map.insert("ARRAY_REMOVE", array_remove);
    map.insert("ARRAY_TO_STRING", array_to_string);
    map.insert("STRING_TO_ARRAY", string_to_array);
    map.insert("UNNEST", unnest);
}

fn array_dimension_arg(value: Option<Value>) -> Option<i32> {
    match value {
        Some(Value::Int32(value)) => Some(value),
        Some(Value::Int64(value)) => i32::try_from(value).ok(),
        None => Some(1),
        _ => None,
    }
}

fn array_recursion_depth_error() -> anyhow::Error {
    anyhow!("array value is too deep")
}

fn array_length_at_dimension(values: &[Value], dim: i32) -> Result<Option<i32>> {
    array_length_at_dimension_inner(values, dim, 0)
}

fn array_length_at_dimension_inner(
    values: &[Value],
    dim: i32,
    depth: usize,
) -> Result<Option<i32>> {
    if dim <= 0 {
        return Ok(None);
    }
    if values.is_empty() {
        return Ok(None);
    }
    if dim == 1 {
        return Ok(i32::try_from(values.len()).ok());
    }
    if depth >= MAX_ARRAY_RECURSION_DEPTH {
        return Err(array_recursion_depth_error());
    }

    let mut expected = None;
    let mut saw_nested = false;
    for value in values {
        match value {
            Value::Array(nested) => {
                saw_nested = true;
                let Some(length) = array_length_at_dimension_inner(nested, dim - 1, depth + 1)?
                else {
                    return Ok(None);
                };
                if expected.is_some_and(|previous| previous != length) {
                    return Ok(None);
                }
                expected = Some(length);
            }
            Value::Null => return Ok(None),
            _ => return Ok(None),
        }
    }

    Ok(saw_nested.then_some(expected).flatten())
}

fn array_cardinality(values: &[Value]) -> Result<Option<i32>> {
    array_cardinality_inner(values, 0)
}

fn array_cardinality_inner(values: &[Value], depth: usize) -> Result<Option<i32>> {
    if depth >= MAX_ARRAY_RECURSION_DEPTH {
        return Err(array_recursion_depth_error());
    }

    let mut count = 0_i32;
    for value in values {
        let addend = match value {
            Value::Array(nested) => {
                let Some(count) = array_cardinality_inner(nested, depth + 1)? else {
                    return Ok(None);
                };
                count
            }
            _ => 1,
        };
        let Some(next_count) = count.checked_add(addend) else {
            return Ok(None);
        };
        count = next_count;
    }
    Ok(Some(count))
}

pub fn array_length(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let arr = match iter.next() {
        Some(Value::Array(a)) => a,
        Some(Value::Null) => return Ok(Value::Null),
        _ => return Ok(Value::Null),
    };
    let Some(dim) = array_dimension_arg(iter.next()) else {
        return Ok(Value::Null);
    };
    Ok(array_length_at_dimension(&arr, dim)?
        .map(Value::Int32)
        .unwrap_or(Value::Null))
}

pub fn array_upper(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let arr = match iter.next() {
        Some(Value::Array(a)) => a,
        Some(Value::Null) => return Ok(Value::Null),
        _ => return Ok(Value::Null),
    };
    let Some(dim) = array_dimension_arg(iter.next()) else {
        return Ok(Value::Null);
    };
    Ok(array_length_at_dimension(&arr, dim)?
        .filter(|length| *length > 0)
        .map(Value::Int32)
        .unwrap_or(Value::Null))
}

pub fn array_lower(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let arr = match iter.next() {
        Some(Value::Array(a)) => a,
        Some(Value::Null) => return Ok(Value::Null),
        _ => return Ok(Value::Null),
    };
    let Some(dim) = array_dimension_arg(iter.next()) else {
        return Ok(Value::Null);
    };
    Ok(array_length_at_dimension(&arr, dim)?
        .filter(|length| *length > 0)
        .map(|_| Value::Int32(1))
        .unwrap_or(Value::Null))
}

pub fn cardinality(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Array(a)) => Ok(array_cardinality(&a)?
            .map(Value::Int32)
            .unwrap_or(Value::Null)),
        Some(Value::Null) => Ok(Value::Null),
        _ => Ok(Value::Null),
    }
}

pub fn array_position(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let arr = match iter.next() {
        Some(Value::Array(a)) => a,
        Some(Value::Null) => return Ok(Value::Null),
        _ => return Ok(Value::Null),
    };
    let Some(shape) = array_shape(&arr)? else {
        return Err(array_multidimensional_unsupported_error(true));
    };
    if shape.ndims() > 1 {
        return Err(array_multidimensional_unsupported_error(true));
    }
    let elem = match iter.next() {
        Some(v) => v,
        None => return Ok(Value::Null),
    };
    let start = match iter.next() {
        Some(Value::Int32(value)) => value,
        Some(Value::Int64(value)) => match i32::try_from(value) {
            Ok(value) => value,
            Err(_) => return Ok(Value::Null),
        },
        Some(Value::Null) => return Err(anyhow!("initial position must not be null")),
        None => 1,
        _ => return Ok(Value::Null),
    };
    let start = start.max(1);
    for (i, v) in arr.iter().enumerate() {
        if (i as i32) + 1 < start {
            continue;
        }
        if crate::sql::expr::compare_values(v, &elem)? == 0 {
            return Ok(Value::Int32((i + 1) as i32));
        }
    }
    Ok(Value::Null)
}

pub fn eq_any(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let array = match iter.next() {
        Some(Value::Array(values)) => values,
        Some(Value::Null) => return Ok(Value::Null),
        _ => return Ok(Value::Null),
    };
    let needle = match iter.next() {
        Some(value) => value,
        None => return Ok(Value::Null),
    };

    if array.is_empty() {
        return Ok(Value::Boolean(false));
    }

    let mut has_null = false;
    for value in &array {
        if matches!(&needle, Value::Null) || matches!(value, Value::Null) {
            has_null = true;
            continue;
        }
        if crate::sql::expr::compare_values(value, &needle)? == 0 {
            return Ok(Value::Boolean(true));
        }
    }

    if has_null {
        Ok(Value::Null)
    } else {
        Ok(Value::Boolean(false))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ArrayShape {
    dims: Vec<usize>,
}

impl ArrayShape {
    fn ndims(&self) -> usize {
        self.dims.len()
    }

    fn is_empty(&self) -> bool {
        self.dims.is_empty()
    }
}

fn array_shape(values: &[Value]) -> Result<Option<ArrayShape>> {
    array_shape_inner(values, false, 0).map(|dims| dims.map(|dims| ArrayShape { dims }))
}

fn array_shape_inner(values: &[Value], nested: bool, depth: usize) -> Result<Option<Vec<usize>>> {
    if values.is_empty() {
        return Ok(Some(if nested { vec![0] } else { vec![] }));
    }
    if depth >= MAX_ARRAY_RECURSION_DEPTH {
        return Err(array_recursion_depth_error());
    }

    let mut saw_array = false;
    let mut saw_scalar = false;
    let mut child_shape: Option<Vec<usize>> = None;

    for value in values {
        match value {
            Value::Array(nested_values) => {
                if saw_scalar {
                    return Ok(None);
                }
                saw_array = true;
                let Some(nested_shape) = array_shape_inner(nested_values, true, depth + 1)? else {
                    return Ok(None);
                };
                match child_shape.as_ref() {
                    Some(expected) if *expected != nested_shape => return Ok(None),
                    None => child_shape = Some(nested_shape),
                    _ => {}
                }
            }
            Value::Null => {}
            _ => {
                if saw_array {
                    return Ok(None);
                }
                saw_scalar = true;
            }
        }
    }

    if saw_array {
        let Some(child_shape) = child_shape else {
            return Ok(None);
        };
        let mut dims = Vec::with_capacity(1 + child_shape.len());
        dims.push(values.len());
        dims.extend(child_shape);
        Ok(Some(dims))
    } else {
        Ok(Some(vec![values.len()]))
    }
}

fn array_value_leaf_type(values: &[Value]) -> Result<Option<DataType>> {
    array_value_leaf_type_inner(values, 0)
}

fn array_value_leaf_type_inner(values: &[Value], depth: usize) -> Result<Option<DataType>> {
    if depth >= MAX_ARRAY_RECURSION_DEPTH {
        return Err(array_recursion_depth_error());
    }

    for value in values {
        match value {
            Value::Array(nested) => {
                if let Some(data_type) = array_value_leaf_type_inner(nested, depth + 1)? {
                    return Ok(Some(data_type));
                }
            }
            Value::Null => {}
            other => {
                if let Some(data_type) = other.data_type() {
                    return Ok(Some(data_type));
                }
            }
        }
    }
    Ok(None)
}

fn array_value_type_name(values: &[Value]) -> Result<String> {
    let base = array_value_leaf_type(values)?
        .unwrap_or(DataType::Text)
        .pg_display_name();
    let rank = array_shape(values)?
        .map(|shape| shape.ndims())
        .unwrap_or(1)
        .max(1);
    Ok(format!("{}{}", base, "[]".repeat(rank)))
}

fn array_cat_common_element_type(left: &DataType, right: &DataType) -> Option<DataType> {
    if matches!(left, DataType::Unknown) {
        return Some(right.clone());
    }
    if matches!(right, DataType::Unknown) {
        return Some(left.clone());
    }

    let common = common_type(left, right)?;
    if is_implicitly_coercible(left, &common) && is_implicitly_coercible(right, &common) {
        Some(common)
    } else {
        None
    }
}

fn array_cat_arg_signature(value: &Value) -> Result<String> {
    match value {
        Value::Array(values) => array_value_type_name(values),
        Value::Null => Ok(DataType::Unknown.pg_display_name()),
        other => Ok(other
            .data_type()
            .unwrap_or(DataType::Unknown)
            .pg_display_name()),
    }
}

fn array_function_not_found(function_name: &str, arg_types: &[String]) -> anyhow::Error {
    SqlError::FunctionNotFound(format!("{}({})", function_name, arg_types.join(", "))).into()
}

fn array_cat_function_not_found(arg_types: &[String]) -> anyhow::Error {
    array_function_not_found("array_cat", arg_types)
}

fn array_cat_incompatible_arrays() -> anyhow::Error {
    SqlError::ArraySubscriptError {
        message: "cannot concatenate incompatible arrays".into(),
    }
    .into()
}

fn array_append_prepend_error() -> anyhow::Error {
    SqlError::ArrayDimensionError {
        message: "argument must be empty or one-dimensional array".into(),
    }
    .into()
}

fn array_multidimensional_unsupported_error(searching: bool) -> anyhow::Error {
    let message = if searching {
        "searching for elements in multidimensional arrays is not supported"
    } else {
        "removing elements from multidimensional arrays is not supported"
    };
    SqlError::Unsupported(message.into()).into()
}

fn array_cat_merge_shapes(
    left: Vec<Value>,
    left_shape: Vec<usize>,
    right: Vec<Value>,
    right_shape: Vec<usize>,
) -> Option<(Vec<Value>, Vec<usize>)> {
    if left_shape.is_empty() {
        return Some((right, right_shape));
    }
    if right_shape.is_empty() {
        return Some((left, left_shape));
    }

    match left_shape.len().cmp(&right_shape.len()) {
        std::cmp::Ordering::Equal => {
            if left_shape.len() > 1 && left_shape[1..] != right_shape[1..] {
                return None;
            }
            let mut result = left;
            result.extend(right);
            let mut shape = left_shape;
            shape[0] += right_shape[0];
            Some((result, shape))
        }
        std::cmp::Ordering::Less if left_shape.len() + 1 == right_shape.len() => {
            if left_shape != right_shape[1..] {
                return None;
            }
            let mut result = Vec::with_capacity(left.len() + right.len());
            result.push(Value::Array(left));
            result.extend(right);
            let mut shape = right_shape;
            shape[0] += 1;
            Some((result, shape))
        }
        std::cmp::Ordering::Greater if right_shape.len() + 1 == left_shape.len() => {
            if right_shape != left_shape[1..] {
                return None;
            }
            let mut result = left;
            result.push(Value::Array(right));
            let mut shape = left_shape;
            shape[0] += 1;
            Some((result, shape))
        }
        _ => None,
    }
}

pub fn array_cat(args: Vec<Value>) -> Result<Value> {
    let arg_types = args
        .iter()
        .map(array_cat_arg_signature)
        .collect::<Result<Vec<_>>>()?;
    let mut result: Option<(Vec<Value>, Vec<usize>, Option<DataType>)> = None;
    let mut saw_array = false;
    for arg in args {
        match arg {
            Value::Array(a) => {
                saw_array = true;
                let Some(shape) = array_shape(&a)? else {
                    return Err(array_cat_incompatible_arrays());
                };
                let array_type = array_value_leaf_type(&a)?;
                match result.as_mut() {
                    None => {
                        result = Some((a, shape.dims, array_type));
                    }
                    Some((existing, existing_shape, existing_type)) => {
                        if existing_shape.is_empty() {
                            if shape.is_empty() {
                                continue;
                            }
                            *existing = a;
                            *existing_shape = shape.dims;
                            *existing_type = array_type;
                            continue;
                        }
                        if shape.is_empty() {
                            continue;
                        }
                        if let (Some(left), Some(right)) =
                            (existing_type.as_ref(), array_type.as_ref())
                        {
                            if array_cat_common_element_type(left, right).is_none() {
                                return Err(array_cat_function_not_found(&arg_types));
                            }
                        }
                        let existing_values = std::mem::take(existing);
                        let existing_shape_values = std::mem::take(existing_shape);
                        let Some((merged, merged_shape)) = array_cat_merge_shapes(
                            existing_values,
                            existing_shape_values,
                            a,
                            shape.dims,
                        ) else {
                            return Err(array_cat_incompatible_arrays());
                        };
                        *existing = merged;
                        *existing_shape = merged_shape;
                        *existing_type = match (existing_type.take(), array_type) {
                            (Some(left), Some(right)) => {
                                array_cat_common_element_type(&left, &right).or(Some(left))
                            }
                            (Some(left), None) => Some(left),
                            (None, Some(right)) => Some(right),
                            (None, None) => None,
                        };
                    }
                }
            }
            Value::Null => {}
            _ => {
                return Err(array_cat_function_not_found(&arg_types));
            }
        }
    }
    if saw_array {
        Ok(Value::Array(
            result.map(|(values, _, _)| values).unwrap_or_default(),
        ))
    } else {
        Ok(Value::Null)
    }
}

pub fn array_append(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let mut arr = match iter.next() {
        Some(Value::Array(a)) => a,
        Some(Value::Null) => Vec::new(),
        _ => return Err(anyhow!("ARRAY_APPEND requires array as first argument")),
    };
    let Some(shape) = array_shape(&arr)? else {
        return Err(array_append_prepend_error());
    };
    if shape.ndims() > 1 {
        return Err(array_append_prepend_error());
    }
    if let Some(elem) = iter.next() {
        arr.push(elem);
    }
    Ok(Value::Array(arr))
}

pub fn array_prepend(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let elem = iter.next().unwrap_or(Value::Null);
    let mut arr = match iter.next() {
        Some(Value::Array(a)) => a,
        Some(Value::Null) => Vec::new(),
        _ => return Err(anyhow!("ARRAY_PREPEND requires array as second argument")),
    };
    let Some(shape) = array_shape(&arr)? else {
        return Err(array_append_prepend_error());
    };
    if shape.ndims() > 1 {
        return Err(array_append_prepend_error());
    }
    arr.insert(0, elem);
    Ok(Value::Array(arr))
}

pub fn array_remove(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let arr = match iter.next() {
        Some(Value::Array(a)) => a,
        Some(Value::Null) => return Ok(Value::Null),
        _ => return Ok(Value::Null),
    };
    let Some(shape) = array_shape(&arr)? else {
        return Err(array_multidimensional_unsupported_error(false));
    };
    if shape.ndims() > 1 {
        return Err(array_multidimensional_unsupported_error(false));
    }
    let elem = match iter.next() {
        Some(v) => v,
        None => return Ok(Value::Array(arr)),
    };
    let mut result = Vec::new();
    for v in arr {
        if crate::sql::expr::compare_values(&v, &elem)? != 0 {
            result.push(v);
        }
    }
    Ok(Value::Array(result))
}

pub fn array_to_string(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let arr = match iter.next() {
        Some(Value::Array(a)) => a,
        Some(Value::Null) => return Ok(Value::Null),
        _ => return Ok(Value::Null),
    };
    let delimiter = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        Some(v) => v.to_string(),
        None => ",".to_string(),
    };
    let null_str = iter.next().and_then(|v| match v {
        Value::Text(s) => Some(s),
        Value::Null => None,
        v => Some(v.to_string()),
    });
    let mut out = String::new();
    let mut first = true;
    append_array_to_string_parts(
        &arr,
        &delimiter,
        null_str.as_deref(),
        &mut out,
        &mut first,
        0,
    )?;
    Ok(Value::Text(out))
}

fn append_array_to_string_parts(
    values: &[Value],
    delimiter: &str,
    null_str: Option<&str>,
    out: &mut String,
    first: &mut bool,
    depth: usize,
) -> Result<()> {
    if depth >= MAX_ARRAY_RECURSION_DEPTH {
        return Err(array_recursion_depth_error());
    }

    for value in values {
        match value {
            Value::Array(nested) => {
                append_array_to_string_parts(nested, delimiter, null_str, out, first, depth + 1)?;
            }
            Value::Null => {
                if let Some(rendered) = null_str {
                    append_array_to_string_part(rendered, delimiter, out, first);
                }
            }
            other => append_array_to_string_part(&other.to_string(), delimiter, out, first),
        }
    }
    Ok(())
}

fn append_array_to_string_part(
    rendered: &str,
    delimiter: &str,
    out: &mut String,
    first: &mut bool,
) {
    if !*first {
        out.push_str(delimiter);
    }
    *first = false;
    out.push_str(rendered);
}

pub fn string_to_array(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let text = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        Some(v) => v.to_string(),
        None => return Ok(Value::Null),
    };
    let delimiter = match iter.next() {
        Some(Value::Text(s)) => Some(s),
        Some(Value::Null) => None,
        Some(v) => Some(v.to_string()),
        None => Some(String::new()),
    };
    let null_str = iter.next().and_then(|v| match v {
        Value::Text(s) => Some(s),
        Value::Null => None,
        v => Some(v.to_string()),
    });

    fn string_to_array_part(part: String, null_str: &Option<String>) -> Value {
        if null_str
            .as_ref()
            .is_some_and(|ns| part.as_str() == ns.as_str())
        {
            Value::Null
        } else {
            Value::Text(part)
        }
    }

    if text.is_empty() {
        if null_str.as_ref().is_some_and(|value| value.is_empty()) {
            return Ok(Value::Array(vec![Value::Null]));
        }
        return Ok(Value::Array(vec![]));
    }

    let parts: Vec<Value> = match delimiter {
        None => text
            .chars()
            .map(|ch| string_to_array_part(ch.to_string(), &null_str))
            .collect(),
        // PostgreSQL treats an empty delimiter as "do not split"; only a NULL
        // delimiter splits the string into characters.
        Some(delimiter) if delimiter.is_empty() => {
            vec![string_to_array_part(text, &null_str)]
        }
        Some(delimiter) => text
            .split(&delimiter)
            .map(|s| string_to_array_part(s.to_string(), &null_str))
            .collect(),
    };
    Ok(Value::Array(parts))
}

pub fn unnest(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Array(arr)) => Ok(Value::Array(arr)),
        Some(Value::Null) | None => Ok(Value::Null),
        Some(v) => Ok(v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nested_array() -> Value {
        Value::Array(vec![
            Value::Array(vec![Value::Int32(1), Value::Int32(2)]),
            Value::Array(vec![Value::Int32(3), Value::Int32(4)]),
        ])
    }

    fn deeply_nested_array(depth: usize) -> Value {
        let mut value = Value::Int32(1);
        for _ in 0..depth {
            value = Value::Array(vec![value]);
        }
        value
    }

    fn wider_nested_array() -> Value {
        Value::Array(vec![
            Value::Array(vec![Value::Int32(5), Value::Int32(6), Value::Int32(7)]),
            Value::Array(vec![Value::Int32(8), Value::Int32(9), Value::Int32(10)]),
        ])
    }

    #[test]
    fn test_array_length() {
        assert_eq!(
            array_length(vec![
                Value::Array(vec![Value::Int32(1), Value::Int32(2), Value::Int32(3)]),
                Value::Int32(1)
            ])
            .unwrap(),
            Value::Int32(3)
        );
    }

    #[test]
    fn test_array_dimension_functions_support_nested_dimensions() {
        assert_eq!(
            array_length(vec![nested_array(), Value::Int32(1)]).unwrap(),
            Value::Int32(2)
        );
        assert_eq!(
            array_length(vec![nested_array(), Value::Int32(2)]).unwrap(),
            Value::Int32(2)
        );
        assert_eq!(
            array_upper(vec![nested_array(), Value::Int32(2)]).unwrap(),
            Value::Int32(2)
        );
        assert_eq!(
            array_lower(vec![nested_array(), Value::Int32(2)]).unwrap(),
            Value::Int32(1)
        );
        assert_eq!(
            array_length(vec![nested_array(), Value::Int32(3)]).unwrap(),
            Value::Null
        );
        assert_eq!(
            array_length(vec![Value::Array(vec![]), Value::Int32(1)]).unwrap(),
            Value::Null
        );
        assert_eq!(
            array_length(vec![nested_array(), Value::Int32(0)]).unwrap(),
            Value::Null
        );
        assert_eq!(
            array_length(vec![nested_array(), Value::Null]).unwrap(),
            Value::Null
        );
        assert_eq!(
            array_length(vec![nested_array(), Value::Int64(i64::MAX)]).unwrap(),
            Value::Null
        );
        assert_eq!(
            array_length(vec![
                Value::Array(vec![
                    Value::Array(vec![Value::Int32(1)]),
                    Value::Array(vec![Value::Int32(2), Value::Int32(3)]),
                ]),
                Value::Int32(2),
            ])
            .unwrap(),
            Value::Null
        );
        assert_eq!(
            array_length(vec![
                Value::Array(vec![Value::Array(vec![Value::Int32(1)]), Value::Null]),
                Value::Int32(2),
            ])
            .unwrap(),
            Value::Null
        );
    }

    #[test]
    fn test_cardinality() {
        assert_eq!(
            cardinality(vec![Value::Array(vec![
                Value::Int32(1),
                Value::Int32(2),
                Value::Int32(3),
                Value::Int32(4)
            ])])
            .unwrap(),
            Value::Int32(4)
        );
        assert_eq!(cardinality(vec![nested_array()]).unwrap(), Value::Int32(4));
        assert_eq!(
            cardinality(vec![Value::Array(vec![
                Value::Array(vec![Value::Int32(1), Value::Null]),
                Value::Array(vec![Value::Int32(3)]),
            ])])
            .unwrap(),
            Value::Int32(3)
        );
        assert_eq!(
            cardinality(vec![Value::Array(vec![])]).unwrap(),
            Value::Int32(0)
        );
    }

    #[test]
    fn test_array_append() {
        assert_eq!(
            array_append(vec![
                Value::Array(vec![Value::Int32(1), Value::Int32(2)]),
                Value::Int32(3)
            ])
            .unwrap(),
            Value::Array(vec![Value::Int32(1), Value::Int32(2), Value::Int32(3)])
        );
    }

    #[test]
    fn test_array_prepend() {
        assert_eq!(
            array_prepend(vec![
                Value::Int32(0),
                Value::Array(vec![Value::Int32(1), Value::Int32(2)])
            ])
            .unwrap(),
            Value::Array(vec![Value::Int32(0), Value::Int32(1), Value::Int32(2)])
        );
    }

    #[test]
    fn test_array_position_rejects_multidimensional_arrays_like_pg() {
        let err = array_position(vec![nested_array(), Value::Int32(1)]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "searching for elements in multidimensional arrays is not supported"
        );
        assert_eq!(
            err.downcast_ref::<crate::sql::error::SqlError>()
                .expect("sql error")
                .sqlstate(),
            "0A000"
        );
    }

    #[test]
    fn test_eq_any_matches_pg_null_semantics() {
        assert_eq!(
            eq_any(vec![
                Value::Array(vec![Value::Int32(1), Value::Null]),
                Value::Int32(1),
            ])
            .unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            eq_any(vec![
                Value::Array(vec![Value::Null, Value::Int32(2)]),
                Value::Int32(1),
            ])
            .unwrap(),
            Value::Null
        );
        assert_eq!(
            eq_any(vec![Value::Array(vec![Value::Null]), Value::Null]).unwrap(),
            Value::Null
        );
        assert_eq!(
            eq_any(vec![Value::Array(vec![]), Value::Null]).unwrap(),
            Value::Boolean(false)
        );
        assert_eq!(
            eq_any(vec![Value::Null, Value::Int32(1)]).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn test_array_remove_rejects_multidimensional_arrays_like_pg() {
        let err = array_remove(vec![nested_array(), Value::Int32(1)]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "removing elements from multidimensional arrays is not supported"
        );
        assert_eq!(
            err.downcast_ref::<crate::sql::error::SqlError>()
                .expect("sql error")
                .sqlstate(),
            "0A000"
        );
    }

    #[test]
    fn test_array_append_and_prepend_reject_multidimensional_arrays_like_pg() {
        let append_err = array_append(vec![nested_array(), Value::Int32(5)]).unwrap_err();
        assert_eq!(
            append_err.to_string(),
            "argument must be empty or one-dimensional array"
        );
        assert_eq!(
            append_err
                .downcast_ref::<crate::sql::error::SqlError>()
                .expect("sql error")
                .sqlstate(),
            "22000"
        );

        let prepend_err = array_prepend(vec![Value::Int32(5), nested_array()]).unwrap_err();
        assert_eq!(
            prepend_err.to_string(),
            "argument must be empty or one-dimensional array"
        );
        assert_eq!(
            prepend_err
                .downcast_ref::<crate::sql::error::SqlError>()
                .expect("sql error")
                .sqlstate(),
            "22000"
        );
    }

    #[test]
    fn test_array_to_string() {
        assert_eq!(
            array_to_string(vec![
                Value::Array(vec![
                    Value::Text("a".into()),
                    Value::Text("b".into()),
                    Value::Text("c".into())
                ]),
                Value::Text(",".into())
            ])
            .unwrap(),
            Value::Text("a,b,c".into())
        );
    }

    #[test]
    fn test_array_helpers_reject_excessive_recursion_depth() {
        let deep = deeply_nested_array(MAX_ARRAY_RECURSION_DEPTH + 1);

        let err = array_to_string(vec![deep.clone(), Value::Text(",".into())]).unwrap_err();
        assert_eq!(err.to_string(), "array value is too deep");

        let err = cardinality(vec![deep.clone()]).unwrap_err();
        assert_eq!(err.to_string(), "array value is too deep");

        let err = array_cat(vec![deep.clone(), Value::Array(vec![])]).unwrap_err();
        assert_eq!(err.to_string(), "array value is too deep");

        let err = array_append(vec![deep.clone(), Value::Int32(2)]).unwrap_err();
        assert_eq!(err.to_string(), "array value is too deep");
    }

    #[test]
    fn test_array_cat_all_null_returns_null() {
        assert_eq!(
            array_cat(vec![Value::Null, Value::Null]).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn test_array_cat_rejects_scalar_arguments_like_pg() {
        let err =
            array_cat(vec![Value::Array(vec![Value::Int32(1)]), Value::Int32(3)]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "function array_cat(integer[], integer) does not exist"
        );
        assert_eq!(
            err.downcast_ref::<crate::sql::error::SqlError>()
                .expect("sql error")
                .sqlstate(),
            "42883"
        );
    }

    #[test]
    fn test_array_cat_uses_text_for_all_null_array_signatures() {
        let err = array_cat(vec![Value::Array(vec![Value::Null]), Value::Int32(3)]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "function array_cat(text[], integer) does not exist"
        );
    }

    #[test]
    fn test_array_cat_accepts_compatible_mixed_arrays_like_pg() {
        assert_eq!(
            array_cat(vec![
                Value::Array(vec![Value::Int32(1)]),
                Value::Array(vec![Value::Int64(2)]),
            ])
            .unwrap(),
            Value::Array(vec![Value::Int32(1), Value::Int64(2)])
        );
    }

    #[test]
    fn test_array_cat_extends_outer_dimensions_like_pg() {
        let one_d = Value::Array(vec![Value::Int32(1), Value::Int32(2)]);
        let two_d = Value::Array(vec![
            Value::Array(vec![Value::Int32(3), Value::Int32(4)]),
            Value::Array(vec![Value::Int32(5), Value::Int32(6)]),
        ]);

        assert_eq!(
            array_cat(vec![one_d.clone(), two_d.clone()]).unwrap(),
            Value::Array(vec![
                Value::Array(vec![Value::Int32(1), Value::Int32(2)]),
                Value::Array(vec![Value::Int32(3), Value::Int32(4)]),
                Value::Array(vec![Value::Int32(5), Value::Int32(6)]),
            ])
        );
        assert_eq!(
            array_cat(vec![two_d.clone(), one_d.clone()]).unwrap(),
            Value::Array(vec![
                Value::Array(vec![Value::Int32(3), Value::Int32(4)]),
                Value::Array(vec![Value::Int32(5), Value::Int32(6)]),
                Value::Array(vec![Value::Int32(1), Value::Int32(2)]),
            ])
        );
    }

    #[test]
    fn test_array_cat_empty_array_is_neutral_like_pg() {
        let empty = Value::Array(vec![]);
        assert_eq!(
            array_cat(vec![empty.clone(), nested_array()]).unwrap(),
            nested_array()
        );
        assert_eq!(
            array_cat(vec![nested_array(), empty]).unwrap(),
            nested_array()
        );
    }

    #[test]
    fn test_array_cat_rejects_incompatible_nested_arrays_like_pg() {
        let err = array_cat(vec![nested_array(), wider_nested_array()]).unwrap_err();
        assert_eq!(err.to_string(), "cannot concatenate incompatible arrays");
        assert_eq!(
            err.downcast_ref::<crate::sql::error::SqlError>()
                .expect("sql error")
                .sqlstate(),
            "2202E"
        );

        let one_d = Value::Array(vec![Value::Int32(1)]);
        let two_d = Value::Array(vec![
            Value::Array(vec![Value::Int32(2), Value::Int32(3)]),
            Value::Array(vec![Value::Int32(4), Value::Int32(5)]),
        ]);
        let err = array_cat(vec![one_d, two_d]).unwrap_err();
        assert_eq!(err.to_string(), "cannot concatenate incompatible arrays");
        assert_eq!(
            err.downcast_ref::<crate::sql::error::SqlError>()
                .expect("sql error")
                .sqlstate(),
            "2202E"
        );
    }

    #[test]
    fn test_array_to_string_null_delimiter_returns_null() {
        assert_eq!(
            array_to_string(vec![
                Value::Array(vec![
                    Value::Text("a".into()),
                    Value::Null,
                    Value::Text("b".into())
                ]),
                Value::Null
            ])
            .unwrap(),
            Value::Null
        );
    }

    #[test]
    fn test_string_to_array_empty_delimiter_keeps_whole_input() {
        assert_eq!(
            string_to_array(vec![Value::Text("abc".into()), Value::Text("".into())]).unwrap(),
            Value::Array(vec![Value::Text("abc".into())])
        );
    }

    #[test]
    fn test_string_to_array_null_string_applies_after_splitting() {
        assert_eq!(
            string_to_array(vec![
                Value::Text("abc".into()),
                Value::Null,
                Value::Text("b".into())
            ])
            .unwrap(),
            Value::Array(vec![
                Value::Text("a".into()),
                Value::Null,
                Value::Text("c".into())
            ])
        );
        assert_eq!(
            string_to_array(vec![
                Value::Text("abc".into()),
                Value::Text("".into()),
                Value::Text("abc".into())
            ])
            .unwrap(),
            Value::Array(vec![Value::Null])
        );
    }

    #[test]
    fn test_string_to_array_empty_input_returns_empty_array() {
        assert_eq!(
            string_to_array(vec![Value::Text("".into()), Value::Text(",".into())]).unwrap(),
            Value::Array(vec![])
        );
        assert_eq!(
            string_to_array(vec![
                Value::Text("".into()),
                Value::Text(",".into()),
                Value::Text("".into())
            ])
            .unwrap(),
            Value::Array(vec![Value::Null])
        );
    }

    #[test]
    fn test_string_to_array() {
        assert_eq!(
            string_to_array(vec![Value::Text("a,b,c".into()), Value::Text(",".into())]).unwrap(),
            Value::Array(vec![
                Value::Text("a".into()),
                Value::Text("b".into()),
                Value::Text("c".into())
            ])
        );
    }
}
