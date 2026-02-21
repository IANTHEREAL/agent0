//! Aggregation logic

use crate::sql::error::SqlError;
use anyhow::{anyhow, Result};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
#[cfg(test)]
use sqlparser::ast::{Expr, Function, FunctionArg, FunctionArgExpr};

use crate::sql::expr::compare_values;
#[cfg(test)]
use crate::sql::names::function_name_upper;
use crate::sql::pg_numeric::pg_numeric_div;
use crate::types::{DataType, Value};

#[cfg(test)]
#[derive(Debug, Clone)]
pub enum AggExpr {
    Function(Function),
    ArrayAgg,
}

#[derive(Debug)]
pub enum Aggregator {
    Count(i64),
    Sum {
        value: Value,
        return_type: DataType,
    },
    Max(Value),
    Min(Value),
    Avg {
        sum: Decimal,
        sum_float: Option<f64>,
        count: i64,
    },
    StringAgg {
        values: Vec<String>,
        delimiter: String,
    },
    ArrayAgg {
        values: Vec<Value>,
    },
    BoolAnd(Option<bool>),
    BoolOr(Option<bool>),
    JsonAgg {
        values: Vec<Value>,
    },
    JsonbAgg {
        values: Vec<Value>,
    },
}

impl Aggregator {
    pub fn new(kind: &str, return_type: Option<DataType>) -> Result<Self> {
        match kind.to_uppercase().as_str() {
            "COUNT" => Ok(Aggregator::Count(0)),
            "SUM" => Ok(Aggregator::Sum {
                value: Value::Null,
                return_type: return_type.unwrap_or(DataType::Numeric {
                    precision: None,
                    scale: None,
                }),
            }),
            "MAX" => Ok(Aggregator::Max(Value::Null)),
            "MIN" => Ok(Aggregator::Min(Value::Null)),
            "AVG" => Ok(Aggregator::Avg {
                sum: Decimal::ZERO,
                sum_float: None,
                count: 0,
            }),
            "STRING_AGG" => Ok(Aggregator::StringAgg {
                values: Vec::new(),
                delimiter: ",".to_string(),
            }),
            "ARRAY_AGG" => Ok(Aggregator::ArrayAgg { values: Vec::new() }),
            "BOOL_AND" | "EVERY" => Ok(Aggregator::BoolAnd(None)),
            "BOOL_OR" => Ok(Aggregator::BoolOr(None)),
            "JSON_AGG" => Ok(Aggregator::JsonAgg { values: Vec::new() }),
            "JSONB_AGG" => Ok(Aggregator::JsonbAgg { values: Vec::new() }),
            _ => Err(
                SqlError::Unsupported(format!("Unsupported aggregate function: {}", kind)).into(),
            ),
        }
    }

    pub fn new_string_agg(delimiter: String) -> Self {
        Aggregator::StringAgg {
            values: Vec::new(),
            delimiter,
        }
    }

    pub fn update(&mut self, val: &Value) -> Result<()> {
        match self {
            Aggregator::Count(_) => {
                if !matches!(val, Value::Null) {
                    if let Aggregator::Count(c) = self {
                        *c += 1;
                    }
                }
            }
            Aggregator::Sum {
                value: current,
                return_type,
            } => {
                if !matches!(val, Value::Null) {
                    if matches!(current, Value::Null) {
                        *current = widen_value(val, return_type);
                    } else {
                        *current = add_values(current, val)?;
                    }
                }
            }
            Aggregator::Max(current) => {
                if !matches!(val, Value::Null) {
                    if matches!(current, Value::Null) {
                        *current = val.clone();
                    } else {
                        if compare_values(val, current)? > 0 {
                            *current = val.clone();
                        }
                    }
                }
            }
            Aggregator::Min(current) => {
                if !matches!(val, Value::Null) {
                    if matches!(current, Value::Null) {
                        *current = val.clone();
                    } else {
                        if compare_values(val, current)? < 0 {
                            *current = val.clone();
                        }
                    }
                }
            }
            Aggregator::Avg {
                sum,
                sum_float,
                count,
            } => {
                if !matches!(val, Value::Null) {
                    match val {
                        Value::Int32(i) => {
                            if let Some(sf) = sum_float.as_mut() {
                                *sf += *i as f64;
                            } else {
                                *sum += Decimal::from(*i);
                            }
                        }
                        Value::Int64(i) => {
                            if let Some(sf) = sum_float.as_mut() {
                                *sf += *i as f64;
                            } else {
                                *sum += Decimal::from(*i);
                            }
                        }
                        Value::Float64(f) => {
                            if sum_float.is_none() {
                                let df = sum.to_f64().ok_or_else(|| {
                                    anyhow!("numeric value out of range for double precision")
                                })?;
                                *sum_float = Some(df);
                            }
                            if let Some(sf) = sum_float.as_mut() {
                                *sf += *f;
                            }
                        }
                        Value::Numeric(d) => {
                            if let Some(sf) = sum_float.as_mut() {
                                let df = d.to_f64().ok_or_else(|| {
                                    anyhow!("numeric value out of range for double precision")
                                })?;
                                *sf += df;
                            } else {
                                *sum += *d;
                            }
                        }
                        _ => return Err(anyhow!("AVG requires numeric type")),
                    }
                    *count += 1;
                }
            }
            Aggregator::StringAgg { values, .. } => {
                if !matches!(val, Value::Null) {
                    let s = match val {
                        Value::Text(s) => s.clone(),
                        v => v.to_string(),
                    };
                    values.push(s);
                }
            }
            Aggregator::ArrayAgg { values } => {
                values.push(val.clone());
            }
            Aggregator::BoolAnd(current) => match val {
                Value::Null => {}
                Value::Boolean(b) => {
                    *current = Some(current.unwrap_or(true) && *b);
                }
                _ => return Err(anyhow!("BOOL_AND requires boolean type")),
            },
            Aggregator::BoolOr(current) => match val {
                Value::Null => {}
                Value::Boolean(b) => {
                    *current = Some(current.unwrap_or(false) || *b);
                }
                _ => return Err(anyhow!("BOOL_OR requires boolean type")),
            },
            Aggregator::JsonAgg { values } => {
                values.push(val.clone());
            }
            Aggregator::JsonbAgg { values } => {
                values.push(val.clone());
            }
        }
        Ok(())
    }

    pub fn result(&self) -> Value {
        match self {
            Aggregator::Count(c) => Value::Int64(*c),
            Aggregator::Sum { value, .. } => value.clone(),
            Aggregator::Max(v) => v.clone(),
            Aggregator::Min(v) => v.clone(),
            Aggregator::Avg {
                sum,
                sum_float,
                count,
            } => {
                if *count == 0 {
                    Value::Null
                } else if let Some(sf) = sum_float {
                    Value::Float64(*sf / *count as f64)
                } else {
                    let denom = Decimal::from(*count);
                    Value::Numeric(pg_numeric_div(*sum, denom))
                }
            }
            Aggregator::StringAgg { values, delimiter } => {
                if values.is_empty() {
                    Value::Null
                } else {
                    Value::Text(values.join(delimiter))
                }
            }
            Aggregator::ArrayAgg { values } => {
                if values.is_empty() {
                    Value::Null
                } else {
                    Value::Array(values.clone())
                }
            }
            Aggregator::BoolAnd(opt) => opt.map_or(Value::Null, Value::Boolean),
            Aggregator::BoolOr(opt) => opt.map_or(Value::Null, Value::Boolean),
            Aggregator::JsonAgg { values } => {
                if values.is_empty() {
                    Value::Null
                } else {
                    // json_agg returns Value::Json which bypasses JSONB output-boundary
                    // canonicalization, so JSONB elements must be canonicalized here.
                    let items: Vec<String> =
                        values.iter().map(value_to_json_str_canonical).collect();
                    Value::Json(format!("[{}]", items.join(",")))
                }
            }
            Aggregator::JsonbAgg { values } => {
                if values.is_empty() {
                    Value::Null
                } else {
                    let items: Vec<String> = values.iter().map(value_to_json_str).collect();
                    // Compact format — output boundary will canonicalize
                    Value::Jsonb(format!("[{}]", items.join(",")))
                }
            }
        }
    }
}

/// Convert a Value to its JSON representation string.
///
/// When `canonicalize_jsonb` is true, `Value::Jsonb` elements at any depth are
/// formatted using PostgreSQL JSONB canonical output (length-first key order,
/// spaced separators). This is needed for JSON_AGG, whose `Value::Json` result
/// bypasses the JSONB output-boundary formatting layer.
fn value_to_json_str_inner(v: &Value, canonicalize_jsonb: bool) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::Int32(i) => i.to_string(),
        Value::Int64(i) => i.to_string(),
        Value::Float64(f) => {
            if f.is_nan() || f.is_infinite() {
                "null".to_string()
            } else {
                f.to_string()
            }
        }
        Value::Numeric(d) => d.to_string(),
        Value::Text(s) => {
            let escaped = s
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
                .replace('\r', "\\r")
                .replace('\t', "\\t");
            format!("\"{}\"", escaped)
        }
        Value::Json(j) => j.clone(),
        Value::Jsonb(j) => {
            if canonicalize_jsonb {
                crate::sql::jsonb::format_jsonb_pg_str(j)
            } else {
                j.clone()
            }
        }
        Value::Bytes(b) => {
            format!("\"\\\\x{}\"", hex::encode(b))
        }
        Value::Array(arr) => {
            let items: Vec<String> = arr
                .iter()
                .map(|v| value_to_json_str_inner(v, canonicalize_jsonb))
                .collect();
            format!("[{}]", items.join(","))
        }
        _ => {
            let s = v.to_string();
            let escaped = s
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
                .replace('\r', "\\r")
                .replace('\t', "\\t");
            format!("\"{}\"", escaped)
        }
    }
}

fn value_to_json_str(v: &Value) -> String {
    value_to_json_str_inner(v, false)
}

fn value_to_json_str_canonical(v: &Value) -> String {
    value_to_json_str_inner(v, true)
}

fn add_values(left: &Value, right: &Value) -> Result<Value> {
    match (left, right) {
        (Value::Int32(l), Value::Int32(r)) => Ok(Value::Int32(l + r)),
        (Value::Int64(l), Value::Int64(r)) => Ok(Value::Int64(l + r)),
        (Value::Int32(l), Value::Int64(r)) => Ok(Value::Int64(*l as i64 + r)),
        (Value::Int64(l), Value::Int32(r)) => Ok(Value::Int64(l + *r as i64)),
        (Value::Float64(l), Value::Float64(r)) => Ok(Value::Float64(l + r)),
        (Value::Int32(l), Value::Float64(r)) => Ok(Value::Float64(*l as f64 + r)),
        (Value::Float64(l), Value::Int32(r)) => Ok(Value::Float64(l + *r as f64)),
        (Value::Int64(l), Value::Float64(r)) => Ok(Value::Float64(*l as f64 + r)),
        (Value::Float64(l), Value::Int64(r)) => Ok(Value::Float64(l + *r as f64)),
        // Handle Text values that can be parsed as numbers (PostgreSQL behavior)
        (Value::Text(l), Value::Text(r)) => {
            // Try parsing as int first, then float
            match (l.parse::<i64>(), r.parse::<i64>()) {
                (Ok(li), Ok(ri)) => Ok(Value::Int64(li + ri)),
                _ => match (l.parse::<f64>(), r.parse::<f64>()) {
                    (Ok(lf), Ok(rf)) => Ok(Value::Float64(lf + rf)),
                    _ => Err(anyhow!("Cannot add non-numeric text values")),
                },
            }
        }
        (Value::Text(t), Value::Int32(i)) | (Value::Int32(i), Value::Text(t)) => {
            if let Ok(ti) = t.parse::<i32>() {
                Ok(Value::Int32(ti + i))
            } else if let Ok(tf) = t.parse::<f64>() {
                Ok(Value::Float64(tf + *i as f64))
            } else {
                Err(anyhow!("Cannot add non-numeric text to number"))
            }
        }
        (Value::Text(t), Value::Int64(i)) | (Value::Int64(i), Value::Text(t)) => {
            if let Ok(ti) = t.parse::<i64>() {
                Ok(Value::Int64(ti + i))
            } else if let Ok(tf) = t.parse::<f64>() {
                Ok(Value::Float64(tf + *i as f64))
            } else {
                Err(anyhow!("Cannot add non-numeric text to number"))
            }
        }
        (Value::Text(t), Value::Float64(f)) | (Value::Float64(f), Value::Text(t)) => {
            if let Ok(tf) = t.parse::<f64>() {
                Ok(Value::Float64(tf + f))
            } else {
                Err(anyhow!("Cannot add non-numeric text to number"))
            }
        }
        (Value::Numeric(l), Value::Numeric(r)) => Ok(Value::Numeric(l + r)),
        (Value::Numeric(d), Value::Int32(i)) | (Value::Int32(i), Value::Numeric(d)) => {
            Ok(Value::Numeric(d + Decimal::from(*i)))
        }
        (Value::Numeric(d), Value::Int64(i)) | (Value::Int64(i), Value::Numeric(d)) => {
            Ok(Value::Numeric(d + Decimal::from(*i)))
        }
        (Value::Numeric(d), Value::Float64(f)) | (Value::Float64(f), Value::Numeric(d)) => {
            let df = d
                .to_f64()
                .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
            Ok(Value::Float64(df + *f))
        }
        _ => Err(SqlError::Unsupported("Unsupported types for SUM".into()).into()),
    }
}

/// Widen a value to the target aggregate return type (safe upcast only, no truncation).
fn widen_value(val: &Value, target: &DataType) -> Value {
    match (val, target) {
        (Value::Int32(v), DataType::Int64) => Value::Int64(*v as i64),
        (Value::Int32(v), DataType::Numeric { .. }) => Value::Numeric(Decimal::from(*v)),
        (Value::Int64(v), DataType::Numeric { .. }) => Value::Numeric(Decimal::from(*v)),
        _ => val.clone(),
    }
}

#[cfg(test)]
/// Collect aggregate functions from HAVING clause that aren't already in projection
pub fn collect_having_agg_funcs(
    expr: &Expr,
    agg_funcs: &mut Vec<(usize, AggExpr)>,
    extra_start: usize,
) {
    match expr {
        Expr::Function(f) if f.over.is_none() => {
            let func_name = function_name_upper(f);
            if matches!(
                func_name.as_str(),
                "COUNT"
                    | "SUM"
                    | "AVG"
                    | "MIN"
                    | "MAX"
                    | "STRING_AGG"
                    | "ARRAY_AGG"
                    | "BOOL_AND"
                    | "BOOL_OR"
                    | "EVERY"
            ) {
                let already_exists = agg_funcs.iter().any(|(_, existing)| {
                    if let AggExpr::Function(existing_f) = existing {
                        let existing_name = function_name_upper(existing_f);
                        existing_name == func_name && args_match(f, existing_f)
                    } else {
                        false
                    }
                });
                if !already_exists {
                    let new_idx = extra_start
                        + (agg_funcs.len()
                            - agg_funcs
                                .iter()
                                .filter(|(idx, _)| *idx < extra_start)
                                .count());
                    agg_funcs.push((new_idx, AggExpr::Function(f.clone())));
                }
            } else {
                for arg in f.args.iter() {
                    if let sqlparser::ast::FunctionArg::Unnamed(
                        sqlparser::ast::FunctionArgExpr::Expr(arg_expr),
                    ) = arg
                    {
                        collect_having_agg_funcs(arg_expr, agg_funcs, extra_start);
                    }
                }
            }
        }
        Expr::ArrayAgg(_) => {
            let already_exists = agg_funcs
                .iter()
                .any(|(_, existing)| matches!(existing, AggExpr::ArrayAgg));
            if !already_exists {
                let new_idx = extra_start
                    + (agg_funcs.len()
                        - agg_funcs
                            .iter()
                            .filter(|(idx, _)| *idx < extra_start)
                            .count());
                agg_funcs.push((new_idx, AggExpr::ArrayAgg));
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_having_agg_funcs(left, agg_funcs, extra_start);
            collect_having_agg_funcs(right, agg_funcs, extra_start);
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if let Some(op) = operand.as_deref() {
                collect_having_agg_funcs(op, agg_funcs, extra_start);
            }
            for cond in conditions {
                collect_having_agg_funcs(cond, agg_funcs, extra_start);
            }
            for res in results {
                collect_having_agg_funcs(res, agg_funcs, extra_start);
            }
            if let Some(e) = else_result.as_deref() {
                collect_having_agg_funcs(e, agg_funcs, extra_start);
            }
        }
        Expr::Nested(e) => collect_having_agg_funcs(e, agg_funcs, extra_start),
        Expr::Cast { expr, .. } => collect_having_agg_funcs(expr, agg_funcs, extra_start),
        _ => {}
    }
}

#[cfg(test)]
/// Check if two function calls match (args + relevant modifiers).
pub fn args_match(f1: &sqlparser::ast::Function, f2: &sqlparser::ast::Function) -> bool {
    // Aggregate modifiers must be part of the match key; otherwise we can accidentally
    // substitute/dedup e.g. COUNT(x) vs COUNT(DISTINCT x), or COUNT(*) vs COUNT(*) FILTER (...).
    if f1.distinct != f2.distinct {
        return false;
    }
    if format!("{:?}", f1.filter) != format!("{:?}", f2.filter) {
        return false;
    }
    if format!("{:?}", f1.order_by) != format!("{:?}", f2.order_by) {
        return false;
    }

    if f1.args.len() != f2.args.len() {
        return false;
    }
    for (a1, a2) in f1.args.iter().zip(f2.args.iter()) {
        match (a1, a2) {
            (
                FunctionArg::Unnamed(FunctionArgExpr::Wildcard),
                FunctionArg::Unnamed(FunctionArgExpr::Wildcard),
            ) => {}
            (
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e1)),
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e2)),
            ) => {
                if format!("{:?}", e1) != format!("{:?}", e2) {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_count() {
        let mut agg = Aggregator::new("COUNT", None).unwrap();
        agg.update(&Value::Int32(1)).unwrap();
        agg.update(&Value::Int32(2)).unwrap();
        agg.update(&Value::Null).unwrap();
        agg.update(&Value::Int32(3)).unwrap();
        assert_eq!(agg.result(), Value::Int64(3));
    }

    #[test]
    fn test_count_empty() {
        let agg = Aggregator::new("COUNT", None).unwrap();
        assert_eq!(agg.result(), Value::Int64(0));
    }

    #[test]
    fn test_sum_int32() {
        // Default return type is Numeric; Int32 values are widened
        let mut agg = Aggregator::new("SUM", None).unwrap();
        agg.update(&Value::Int32(10)).unwrap();
        agg.update(&Value::Int32(20)).unwrap();
        agg.update(&Value::Int32(30)).unwrap();
        assert_eq!(agg.result(), Value::Numeric(Decimal::from(60)));
    }

    #[test]
    fn test_sum_with_null() {
        let mut agg = Aggregator::new("SUM", None).unwrap();
        agg.update(&Value::Int32(10)).unwrap();
        agg.update(&Value::Null).unwrap();
        agg.update(&Value::Int32(20)).unwrap();
        assert_eq!(agg.result(), Value::Numeric(Decimal::from(30)));
    }

    #[test]
    fn test_sum_empty() {
        let agg = Aggregator::new("SUM", None).unwrap();
        assert_eq!(agg.result(), Value::Null);
    }

    #[test]
    fn test_max() {
        let mut agg = Aggregator::new("MAX", None).unwrap();
        agg.update(&Value::Int32(5)).unwrap();
        agg.update(&Value::Int32(10)).unwrap();
        agg.update(&Value::Int32(3)).unwrap();
        assert_eq!(agg.result(), Value::Int32(10));
    }

    #[test]
    fn test_max_with_null() {
        let mut agg = Aggregator::new("MAX", None).unwrap();
        agg.update(&Value::Null).unwrap();
        agg.update(&Value::Int32(5)).unwrap();
        agg.update(&Value::Null).unwrap();
        assert_eq!(agg.result(), Value::Int32(5));
    }

    #[test]
    fn test_min() {
        let mut agg = Aggregator::new("MIN", None).unwrap();
        agg.update(&Value::Int32(5)).unwrap();
        agg.update(&Value::Int32(2)).unwrap();
        agg.update(&Value::Int32(8)).unwrap();
        assert_eq!(agg.result(), Value::Int32(2));
    }

    #[test]
    fn test_avg() {
        let mut agg = Aggregator::new("AVG", None).unwrap();
        agg.update(&Value::Int32(10)).unwrap();
        agg.update(&Value::Int32(20)).unwrap();
        agg.update(&Value::Int32(30)).unwrap();
        let result = agg.result();
        assert_eq!(result, Value::Numeric(Decimal::from(20)));
    }

    #[test]
    fn test_avg_with_null() {
        let mut agg = Aggregator::new("AVG", None).unwrap();
        agg.update(&Value::Int32(10)).unwrap();
        agg.update(&Value::Null).unwrap();
        agg.update(&Value::Int32(20)).unwrap();
        let result = agg.result();
        assert_eq!(result, Value::Numeric(Decimal::from(15)));
    }

    #[test]
    fn test_avg_empty() {
        let agg = Aggregator::new("AVG", None).unwrap();
        assert_eq!(agg.result(), Value::Null);
    }

    #[test]
    fn test_avg_float() {
        let mut agg = Aggregator::new("AVG", None).unwrap();
        agg.update(&Value::Float64(1.5)).unwrap();
        agg.update(&Value::Float64(2.5)).unwrap();
        let result = agg.result();
        assert_eq!(result, Value::Float64(2.0));
    }

    #[test]
    fn test_avg_int_repeating_precision() {
        let mut agg = Aggregator::new("AVG", None).unwrap();
        agg.update(&Value::Int32(300)).unwrap();
        agg.update(&Value::Int32(200)).unwrap();
        agg.update(&Value::Int32(300)).unwrap();
        let result = agg.result();
        assert_eq!(
            result,
            Value::Numeric(Decimal::from_str_exact("266.6666666666666667").unwrap())
        );
    }

    #[test]
    fn test_unsupported_aggregator() {
        assert!(Aggregator::new("UNKNOWN", None).is_err());
    }

    #[test]
    fn test_max_text() {
        let mut agg = Aggregator::new("MAX", None).unwrap();
        agg.update(&Value::Text("apple".to_string())).unwrap();
        agg.update(&Value::Text("banana".to_string())).unwrap();
        agg.update(&Value::Text("cherry".to_string())).unwrap();
        assert_eq!(agg.result(), Value::Text("cherry".to_string()));
    }

    #[test]
    fn test_min_text() {
        let mut agg = Aggregator::new("MIN", None).unwrap();
        agg.update(&Value::Text("banana".to_string())).unwrap();
        agg.update(&Value::Text("apple".to_string())).unwrap();
        agg.update(&Value::Text("cherry".to_string())).unwrap();
        assert_eq!(agg.result(), Value::Text("apple".to_string()));
    }

    #[test]
    fn test_string_agg() {
        let mut agg = Aggregator::new_string_agg(", ".to_string());
        agg.update(&Value::Text("apple".to_string())).unwrap();
        agg.update(&Value::Text("banana".to_string())).unwrap();
        agg.update(&Value::Text("cherry".to_string())).unwrap();
        assert_eq!(
            agg.result(),
            Value::Text("apple, banana, cherry".to_string())
        );
    }

    #[test]
    fn test_string_agg_with_null() {
        let mut agg = Aggregator::new_string_agg(",".to_string());
        agg.update(&Value::Text("a".to_string())).unwrap();
        agg.update(&Value::Null).unwrap();
        agg.update(&Value::Text("b".to_string())).unwrap();
        assert_eq!(agg.result(), Value::Text("a,b".to_string()));
    }

    #[test]
    fn test_string_agg_empty() {
        let agg = Aggregator::new_string_agg(",".to_string());
        assert_eq!(agg.result(), Value::Null);
    }

    #[test]
    fn test_array_agg() {
        let mut agg = Aggregator::new("ARRAY_AGG", None).unwrap();
        agg.update(&Value::Int32(1)).unwrap();
        agg.update(&Value::Int32(2)).unwrap();
        agg.update(&Value::Int32(3)).unwrap();
        assert_eq!(
            agg.result(),
            Value::Array(vec![Value::Int32(1), Value::Int32(2), Value::Int32(3)])
        );
    }

    #[test]
    fn test_array_agg_with_null() {
        let mut agg = Aggregator::new("ARRAY_AGG", None).unwrap();
        agg.update(&Value::Int32(1)).unwrap();
        agg.update(&Value::Null).unwrap();
        agg.update(&Value::Int32(2)).unwrap();
        assert_eq!(
            agg.result(),
            Value::Array(vec![Value::Int32(1), Value::Null, Value::Int32(2)])
        );
    }

    #[test]
    fn test_array_agg_empty() {
        let agg = Aggregator::new("ARRAY_AGG", None).unwrap();
        assert_eq!(agg.result(), Value::Null);
    }

    #[test]
    fn test_jsonb_agg_returns_jsonb_type() {
        let mut agg = Aggregator::new("JSONB_AGG", None).unwrap();
        agg.update(&Value::Int32(1)).unwrap();
        agg.update(&Value::Int32(2)).unwrap();
        agg.update(&Value::Int32(3)).unwrap();
        match agg.result() {
            Value::Jsonb(s) => assert_eq!(s, "[1,2,3]"),
            other => panic!("expected Value::Jsonb, got {:?}", other),
        }
    }

    #[test]
    fn test_jsonb_agg_with_jsonb_inputs() {
        let mut agg = Aggregator::new("JSONB_AGG", None).unwrap();
        agg.update(&Value::Jsonb(r#"{"a":1}"#.to_string())).unwrap();
        agg.update(&Value::Jsonb(r#"{"b":2}"#.to_string())).unwrap();
        match agg.result() {
            Value::Jsonb(s) => assert_eq!(s, r#"[{"a":1},{"b":2}]"#),
            other => panic!("expected Value::Jsonb, got {:?}", other),
        }
    }

    #[test]
    fn test_jsonb_agg_empty() {
        let agg = Aggregator::new("JSONB_AGG", None).unwrap();
        assert_eq!(agg.result(), Value::Null);
    }

    #[test]
    fn test_json_agg_returns_json_type() {
        let mut agg = Aggregator::new("JSON_AGG", None).unwrap();
        agg.update(&Value::Int32(1)).unwrap();
        agg.update(&Value::Int32(2)).unwrap();
        match agg.result() {
            Value::Json(s) => assert_eq!(s, "[1,2]"),
            other => panic!("expected Value::Json, got {:?}", other),
        }
    }

    #[test]
    fn test_json_agg_with_jsonb_inputs_canonical() {
        // json_agg(jsonb_col) must canonicalize JSONB elements inside the JSON array,
        // because the result is Value::Json which bypasses JSONB output-boundary formatting.
        // PG 17.7: SELECT json_agg(v) FROM (VALUES ('{"color":"w","size":"M"}'::jsonb)) t(v);
        //       => [{"size": "M", "color": "w"}]
        let mut agg = Aggregator::new("JSON_AGG", None).unwrap();
        agg.update(&Value::Jsonb(r#"{"color":"w","size":"M"}"#.to_string()))
            .unwrap();
        match agg.result() {
            Value::Json(s) => assert_eq!(s, r#"[{"size": "M", "color": "w"}]"#),
            other => panic!("expected Value::Json, got {:?}", other),
        }
    }

    #[test]
    fn test_json_agg_with_nested_jsonb_array_canonical() {
        // json_agg(jsonb[]) must canonicalize JSONB elements at any nesting depth.
        // PG 17.7: SELECT json_agg(arr) FROM (SELECT ARRAY['{"color":"w","size":"M"}'::jsonb]) t(arr);
        //       => [[{"size": "M", "color": "w"}]]
        let mut agg = Aggregator::new("JSON_AGG", None).unwrap();
        agg.update(&Value::Array(vec![Value::Jsonb(
            r#"{"color":"w","size":"M"}"#.to_string(),
        )]))
        .unwrap();
        match agg.result() {
            Value::Json(s) => assert_eq!(s, r#"[[{"size": "M", "color": "w"}]]"#),
            other => panic!("expected Value::Json, got {:?}", other),
        }
    }

    fn parse_first_projection_function(sql: &str) -> sqlparser::ast::Function {
        use sqlparser::dialect::PostgreSqlDialect;
        use sqlparser::parser::Parser;
        let dialect = PostgreSqlDialect {};
        let statements = Parser::parse_sql(&dialect, sql).unwrap();
        let sqlparser::ast::Statement::Query(query) = &statements[0] else {
            panic!("expected query");
        };
        let sqlparser::ast::SetExpr::Select(select) = &*query.body else {
            panic!("expected select");
        };
        match &select.projection[0] {
            sqlparser::ast::SelectItem::UnnamedExpr(sqlparser::ast::Expr::Function(f))
            | sqlparser::ast::SelectItem::ExprWithAlias {
                expr: sqlparser::ast::Expr::Function(f),
                ..
            } => f.clone(),
            other => panic!("expected function projection, got: {other:?}"),
        }
    }

    fn parse_first_projection_expr(sql: &str) -> sqlparser::ast::Expr {
        use sqlparser::dialect::PostgreSqlDialect;
        use sqlparser::parser::Parser;
        let dialect = PostgreSqlDialect {};
        let statements = Parser::parse_sql(&dialect, sql).unwrap();
        let sqlparser::ast::Statement::Query(query) = &statements[0] else {
            panic!("expected query");
        };
        let sqlparser::ast::SetExpr::Select(select) = &*query.body else {
            panic!("expected select");
        };
        match &select.projection[0] {
            sqlparser::ast::SelectItem::UnnamedExpr(expr)
            | sqlparser::ast::SelectItem::ExprWithAlias { expr, .. } => expr.clone(),
            other => panic!("expected projection expr, got: {other:?}"),
        }
    }

    #[test]
    fn test_args_match_considers_distinct_filter_and_order_by() {
        let count_x = parse_first_projection_function("SELECT COUNT(x)");
        let count_distinct_x = parse_first_projection_function("SELECT COUNT(DISTINCT x)");
        assert!(!args_match(&count_x, &count_distinct_x));

        let count_star = parse_first_projection_function("SELECT COUNT(*)");
        let count_star_filter =
            parse_first_projection_function("SELECT COUNT(*) FILTER (WHERE x > 0)");
        assert!(!args_match(&count_star, &count_star_filter));

        let count_star_filter_same =
            parse_first_projection_function("SELECT COUNT(*) FILTER (WHERE x > 0)");
        assert!(args_match(&count_star_filter, &count_star_filter_same));

        let string_agg_order_asc =
            parse_first_projection_function("SELECT STRING_AGG(x, ',' ORDER BY x)");
        let string_agg_order_desc =
            parse_first_projection_function("SELECT STRING_AGG(x, ',' ORDER BY x DESC)");
        assert!(!args_match(&string_agg_order_asc, &string_agg_order_desc));
    }

    #[test]
    fn test_collect_having_agg_funcs_does_not_dedup_distinct_or_filtered_aggregates() {
        let count_x = parse_first_projection_function("SELECT COUNT(x)");
        let mut agg_funcs = vec![(0, AggExpr::Function(count_x))];

        let count_distinct_x_expr = parse_first_projection_expr("SELECT COUNT(DISTINCT x) > 0");
        collect_having_agg_funcs(&count_distinct_x_expr, &mut agg_funcs, 1);
        assert_eq!(agg_funcs.len(), 2);

        let count_star = parse_first_projection_function("SELECT COUNT(*)");
        let mut agg_funcs = vec![(0, AggExpr::Function(count_star))];
        let count_star_filter_expr =
            parse_first_projection_expr("SELECT COUNT(*) FILTER (WHERE x > 0) > 0");
        collect_having_agg_funcs(&count_star_filter_expr, &mut agg_funcs, 1);
        assert_eq!(agg_funcs.len(), 2);
    }

    #[test]
    fn test_sum_int32_returns_int64_with_return_type() {
        let mut agg = Aggregator::new("SUM", Some(DataType::Int64)).unwrap();
        agg.update(&Value::Int32(10)).unwrap();
        agg.update(&Value::Int32(20)).unwrap();
        agg.update(&Value::Int32(30)).unwrap();
        assert_eq!(agg.result(), Value::Int64(60));
    }

    #[test]
    fn test_sum_int64_returns_numeric_with_return_type() {
        let mut agg = Aggregator::new(
            "SUM",
            Some(DataType::Numeric {
                precision: None,
                scale: None,
            }),
        )
        .unwrap();
        agg.update(&Value::Int64(100)).unwrap();
        agg.update(&Value::Int64(200)).unwrap();
        assert_eq!(agg.result(), Value::Numeric(Decimal::from(300)));
    }
}
