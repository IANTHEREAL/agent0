//! Aggregation logic

use crate::sql::expr::compare_values;
use crate::types::Value;
use anyhow::{anyhow, Result};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;

#[derive(Debug)]
pub enum Aggregator {
    Count(i64),
    Sum(Value),
    Max(Value),
    Min(Value),
    Avg {
        sum: Decimal,
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
}

impl Aggregator {
    pub fn new(kind: &str) -> Result<Self> {
        match kind.to_uppercase().as_str() {
            "COUNT" => Ok(Aggregator::Count(0)),
            "SUM" => Ok(Aggregator::Sum(Value::Null)),
            "MAX" => Ok(Aggregator::Max(Value::Null)),
            "MIN" => Ok(Aggregator::Min(Value::Null)),
            "AVG" => Ok(Aggregator::Avg {
                sum: Decimal::ZERO,
                count: 0,
            }),
            "STRING_AGG" => Ok(Aggregator::StringAgg {
                values: Vec::new(),
                delimiter: ",".to_string(),
            }),
            "ARRAY_AGG" => Ok(Aggregator::ArrayAgg { values: Vec::new() }),
            "BOOL_AND" | "EVERY" => Ok(Aggregator::BoolAnd(None)),
            "BOOL_OR" => Ok(Aggregator::BoolOr(None)),
            _ => Err(anyhow!("Unsupported aggregate function: {}", kind)),
        }
    }

    pub fn new_string_agg(delimiter: String) -> Self {
        Aggregator::StringAgg {
            values: Vec::new(),
            delimiter,
        }
    }

    pub fn new_array_agg() -> Self {
        Aggregator::ArrayAgg { values: Vec::new() }
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
            Aggregator::Sum(current) => {
                if !matches!(val, Value::Null) {
                    if matches!(current, Value::Null) {
                        *current = val.clone();
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
            Aggregator::Avg { sum, count } => {
                if !matches!(val, Value::Null) {
                    let v = match val {
                        Value::Int32(i) => Decimal::from(*i),
                        Value::Int64(i) => Decimal::from(*i),
                        Value::Float64(f) => Decimal::try_from(*f).unwrap_or(Decimal::ZERO),
                        Value::Numeric(d) => *d,
                        _ => return Err(anyhow!("AVG requires numeric type")),
                    };
                    *sum += v;
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
            Aggregator::BoolAnd(current) => {
                if let Value::Boolean(b) = val {
                    *current = Some(current.unwrap_or(true) && *b);
                }
            }
            Aggregator::BoolOr(current) => {
                if let Value::Boolean(b) = val {
                    *current = Some(current.unwrap_or(false) || *b);
                }
            }
        }
        Ok(())
    }

    pub fn result(&self) -> Value {
        match self {
            Aggregator::Count(c) => Value::Int64(*c),
            Aggregator::Sum(v) => v.clone(),
            Aggregator::Max(v) => v.clone(),
            Aggregator::Min(v) => v.clone(),
            Aggregator::Avg { sum, count } => {
                if *count == 0 {
                    Value::Null
                } else {
                    Value::Numeric(*sum / Decimal::from(*count))
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
        }
    }
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
        _ => Err(anyhow!("Unsupported types for SUM")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_count() {
        let mut agg = Aggregator::new("COUNT").unwrap();
        agg.update(&Value::Int32(1)).unwrap();
        agg.update(&Value::Int32(2)).unwrap();
        agg.update(&Value::Null).unwrap();
        agg.update(&Value::Int32(3)).unwrap();
        assert_eq!(agg.result(), Value::Int64(3));
    }

    #[test]
    fn test_count_empty() {
        let agg = Aggregator::new("COUNT").unwrap();
        assert_eq!(agg.result(), Value::Int64(0));
    }

    #[test]
    fn test_sum_int32() {
        let mut agg = Aggregator::new("SUM").unwrap();
        agg.update(&Value::Int32(10)).unwrap();
        agg.update(&Value::Int32(20)).unwrap();
        agg.update(&Value::Int32(30)).unwrap();
        assert_eq!(agg.result(), Value::Int32(60));
    }

    #[test]
    fn test_sum_with_null() {
        let mut agg = Aggregator::new("SUM").unwrap();
        agg.update(&Value::Int32(10)).unwrap();
        agg.update(&Value::Null).unwrap();
        agg.update(&Value::Int32(20)).unwrap();
        assert_eq!(agg.result(), Value::Int32(30));
    }

    #[test]
    fn test_sum_empty() {
        let agg = Aggregator::new("SUM").unwrap();
        assert_eq!(agg.result(), Value::Null);
    }

    #[test]
    fn test_max() {
        let mut agg = Aggregator::new("MAX").unwrap();
        agg.update(&Value::Int32(5)).unwrap();
        agg.update(&Value::Int32(10)).unwrap();
        agg.update(&Value::Int32(3)).unwrap();
        assert_eq!(agg.result(), Value::Int32(10));
    }

    #[test]
    fn test_max_with_null() {
        let mut agg = Aggregator::new("MAX").unwrap();
        agg.update(&Value::Null).unwrap();
        agg.update(&Value::Int32(5)).unwrap();
        agg.update(&Value::Null).unwrap();
        assert_eq!(agg.result(), Value::Int32(5));
    }

    #[test]
    fn test_min() {
        let mut agg = Aggregator::new("MIN").unwrap();
        agg.update(&Value::Int32(5)).unwrap();
        agg.update(&Value::Int32(2)).unwrap();
        agg.update(&Value::Int32(8)).unwrap();
        assert_eq!(agg.result(), Value::Int32(2));
    }

    #[test]
    fn test_avg() {
        let mut agg = Aggregator::new("AVG").unwrap();
        agg.update(&Value::Int32(10)).unwrap();
        agg.update(&Value::Int32(20)).unwrap();
        agg.update(&Value::Int32(30)).unwrap();
        let result = agg.result();
        assert_eq!(result, Value::Numeric(Decimal::from(20)));
    }

    #[test]
    fn test_avg_with_null() {
        let mut agg = Aggregator::new("AVG").unwrap();
        agg.update(&Value::Int32(10)).unwrap();
        agg.update(&Value::Null).unwrap();
        agg.update(&Value::Int32(20)).unwrap();
        let result = agg.result();
        assert_eq!(result, Value::Numeric(Decimal::from(15)));
    }

    #[test]
    fn test_avg_empty() {
        let agg = Aggregator::new("AVG").unwrap();
        assert_eq!(agg.result(), Value::Null);
    }

    #[test]
    fn test_avg_float() {
        let mut agg = Aggregator::new("AVG").unwrap();
        agg.update(&Value::Float64(1.5)).unwrap();
        agg.update(&Value::Float64(2.5)).unwrap();
        let result = agg.result();
        assert_eq!(result, Value::Numeric(Decimal::from(2)));
    }

    #[test]
    fn test_avg_int_repeating_precision() {
        let mut agg = Aggregator::new("AVG").unwrap();
        agg.update(&Value::Int32(300)).unwrap();
        agg.update(&Value::Int32(200)).unwrap();
        agg.update(&Value::Int32(300)).unwrap();
        let result = agg.result();
        assert_eq!(
            result,
            Value::Numeric(Decimal::from_str_exact(
                "266.66666666666666666666666667"
            )
            .unwrap())
        );
    }

    #[test]
    fn test_unsupported_aggregator() {
        assert!(Aggregator::new("UNKNOWN").is_err());
    }

    #[test]
    fn test_max_text() {
        let mut agg = Aggregator::new("MAX").unwrap();
        agg.update(&Value::Text("apple".to_string())).unwrap();
        agg.update(&Value::Text("banana".to_string())).unwrap();
        agg.update(&Value::Text("cherry".to_string())).unwrap();
        assert_eq!(agg.result(), Value::Text("cherry".to_string()));
    }

    #[test]
    fn test_min_text() {
        let mut agg = Aggregator::new("MIN").unwrap();
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
        let mut agg = Aggregator::new_array_agg();
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
        let mut agg = Aggregator::new_array_agg();
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
        let agg = Aggregator::new_array_agg();
        assert_eq!(agg.result(), Value::Null);
    }
}
