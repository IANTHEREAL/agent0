//! Aggregation logic

use crate::sql::error::SqlError;
use anyhow::{anyhow, Result};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
#[cfg(test)]
use sqlparser::ast::{Expr, Function, FunctionArg, FunctionArgExpr};

use crate::model::{DataType, Value};
use crate::sql::expr::compare_values;
use crate::sql::memory::estimate_value_size;
#[cfg(test)]
use crate::sql::names::function_name_upper;
use crate::sql::pg_numeric::pg_numeric_div;
use crate::sql::vector::validate_vector;

fn vector_value(vec: Vec<f64>) -> Result<Value> {
    Ok(Value::Vector(validate_vector(vec, 0)?))
}

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
        sum_vector: Option<Vec<f64>>,
        count: i64,
    },
    StringAgg {
        /// Each entry is `(value, delimiter)`. For i > 0, delimiter[i] is
        /// placed before value[i] (i.e., between value[i-1] and value[i]).
        /// The first row's delimiter is unused. Matches PostgreSQL semantics.
        entries: Vec<(String, String)>,
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

/// Conservative retained-memory delta for aggregate state updates.
///
/// This is intentionally not allocator-exact. It must stay O(1) in the current
/// state size and avoid unbounded under-counting of retained aggregate state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AggregateStateDelta {
    pub grow_bytes: usize,
    pub shrink_bytes: usize,
}

impl AggregateStateDelta {
    fn none() -> Self {
        Self::default()
    }

    fn grow(bytes: usize) -> Self {
        Self {
            grow_bytes: bytes,
            shrink_bytes: 0,
        }
    }

    fn from_sizes(before: usize, after: usize) -> Self {
        if after >= before {
            Self::grow(after - before)
        } else {
            Self {
                grow_bytes: 0,
                shrink_bytes: before - after,
            }
        }
    }
}

fn vector_state_size(v: &[f64]) -> usize {
    std::mem::size_of::<Vec<f64>>() + std::mem::size_of_val(v)
}

/// Determine a `string_agg` input's retained length without building the
/// retained string when possible. Text values report their length from the
/// borrowed value so admission can precede the clone. Other types must be
/// formatted to learn their length — an **explicitly bounded exception**:
/// one value-sized transient that is freed if the charge rejects it and is
/// never retained without admission.
fn string_agg_payload(val: &Value) -> (usize, Option<String>) {
    match val {
        Value::Text(s) => (s.len(), None),
        v => {
            let s = v.to_string();
            (s.len(), Some(s))
        }
    }
}

/// Admit an accumulating push **before** anything is allocated: the payload
/// is known from the existing input value, and when the Vec is full the next
/// capacity is chosen explicitly (doubling) so the slot growth is a measured
/// decision, not a prediction of std's private policy. Returns the admitted
/// total and the reservation to make; on charge failure nothing was
/// allocated. Without this, a full retained Vec's reallocation creates a
/// state-sized backing buffer ahead of quota admission.
fn admit_accumulating_push<T>(
    vec: &Vec<T>,
    payload_bytes: usize,
    charge: &mut dyn FnMut(usize) -> Result<()>,
) -> Result<(usize, usize)> {
    let needs_growth = vec.len() == vec.capacity();
    let target_capacity = if needs_growth {
        std::cmp::max(4, vec.capacity().saturating_mul(2))
    } else {
        vec.capacity()
    };
    let grown_slots = target_capacity
        .saturating_sub(vec.capacity())
        .saturating_mul(std::mem::size_of::<T>());
    let total = payload_bytes.saturating_add(grown_slots);
    charge(total)?;
    let additional = if needs_growth {
        target_capacity - vec.len()
    } else {
        0
    };
    Ok((total, additional))
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
                sum_vector: None,
                count: 0,
            }),
            "STRING_AGG" => Ok(Aggregator::StringAgg {
                entries: Vec::new(),
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

    pub fn new_string_agg() -> Self {
        Aggregator::StringAgg {
            entries: Vec::new(),
        }
    }

    /// Append a `(value, delimiter)` pair for `string_agg`.
    /// The delimiter is evaluated per-row to match PostgreSQL semantics.
    #[allow(dead_code)] // quota-free convenience wrapper; production folds go through *_charged
    pub fn update_string_agg(
        &mut self,
        val: &Value,
        delimiter: &str,
    ) -> Result<AggregateStateDelta> {
        self.update_string_agg_charged(val, delimiter, &mut |_| Ok(()))
    }

    /// Like [`Self::update_string_agg`], admitting retained growth through
    /// `charge` before it is allocated. The returned delta's `grow_bytes`
    /// were already passed to `charge`; callers record them, they do not
    /// re-charge them.
    pub fn update_string_agg_charged(
        &mut self,
        val: &Value,
        delimiter: &str,
        charge: &mut dyn FnMut(usize) -> Result<()>,
    ) -> Result<AggregateStateDelta> {
        if let Aggregator::StringAgg { entries } = self {
            if !matches!(val, Value::Null) {
                let (payload_len, prebuilt) = string_agg_payload(val);
                let retained_bytes = payload_len.saturating_add(delimiter.len());
                let (admitted, additional) =
                    admit_accumulating_push(entries, retained_bytes, charge)?;
                if additional > 0 {
                    entries.reserve_exact(additional);
                }
                // The retained clone is built only after admission.
                let s = match prebuilt {
                    Some(s) => s,
                    None => {
                        let Value::Text(s) = val else {
                            return Err(anyhow!("string_agg input changed type mid-update"));
                        };
                        s.clone()
                    }
                };
                entries.push((s, delimiter.to_string()));
                return Ok(AggregateStateDelta::grow(admitted));
            }
            Ok(AggregateStateDelta::none())
        } else {
            Err(anyhow!(
                "update_string_agg called on non-StringAgg aggregator"
            ))
        }
    }

    #[allow(dead_code)] // quota-free convenience wrapper; production folds go through *_charged
    pub fn update(&mut self, val: &Value) -> Result<AggregateStateDelta> {
        self.update_charged(val, &mut |_| Ok(()))
    }

    /// Like [`Self::update`], admitting retained growth through `charge`.
    /// Accumulating aggregators (array/json/jsonb/string agg) admit payload
    /// and explicit slot growth **before** anything is allocated; fixed-size
    /// aggregators charge their measured value-sized delta after the fold
    /// (a value-sized transient, within contract). The returned delta's
    /// `grow_bytes` were already passed to `charge`; callers record them,
    /// they do not re-charge them. Shrinks are returned for the caller to
    /// release.
    pub fn update_charged(
        &mut self,
        val: &Value,
        charge: &mut dyn FnMut(usize) -> Result<()>,
    ) -> Result<AggregateStateDelta> {
        let delta = match self {
            Aggregator::Count(_) => {
                if !matches!(val, Value::Null) {
                    if let Aggregator::Count(c) = self {
                        *c += 1;
                    }
                }
                AggregateStateDelta::none()
            }
            Aggregator::Sum {
                value: current,
                return_type,
            } => {
                if !matches!(val, Value::Null) {
                    let before = estimate_value_size(current);
                    if matches!(current, Value::Null) {
                        *current = widen_value(val, return_type);
                    } else {
                        *current = add_values(current, val)?;
                    }
                    let delta =
                        AggregateStateDelta::from_sizes(before, estimate_value_size(current));
                    if delta.grow_bytes > 0 {
                        charge(delta.grow_bytes)?;
                    }
                    delta
                } else {
                    AggregateStateDelta::none()
                }
            }
            Aggregator::Max(current) => {
                if !matches!(val, Value::Null)
                    && (matches!(current, Value::Null) || compare_values(val, current)? > 0)
                {
                    let before = estimate_value_size(current);
                    *current = val.clone();
                    let delta =
                        AggregateStateDelta::from_sizes(before, estimate_value_size(current));
                    if delta.grow_bytes > 0 {
                        charge(delta.grow_bytes)?;
                    }
                    delta
                } else {
                    AggregateStateDelta::none()
                }
            }
            Aggregator::Min(current) => {
                if !matches!(val, Value::Null)
                    && (matches!(current, Value::Null) || compare_values(val, current)? < 0)
                {
                    let before = estimate_value_size(current);
                    *current = val.clone();
                    let delta =
                        AggregateStateDelta::from_sizes(before, estimate_value_size(current));
                    if delta.grow_bytes > 0 {
                        charge(delta.grow_bytes)?;
                    }
                    delta
                } else {
                    AggregateStateDelta::none()
                }
            }
            Aggregator::Avg {
                sum,
                sum_float,
                sum_vector,
                count,
            } => {
                if !matches!(val, Value::Null) {
                    let before = sum_vector.as_deref().map(vector_state_size).unwrap_or(0);
                    match val {
                        Value::Int32(i) => {
                            if let Some(sf) = sum_float.as_mut() {
                                *sf += *i as f64;
                            } else {
                                *sum = crate::sql::expr::numeric::checked_decimal_add(
                                    *sum,
                                    Decimal::from(*i),
                                )?;
                            }
                        }
                        Value::Int64(i) => {
                            if let Some(sf) = sum_float.as_mut() {
                                *sf += *i as f64;
                            } else {
                                *sum = crate::sql::expr::numeric::checked_decimal_add(
                                    *sum,
                                    Decimal::from(*i),
                                )?;
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
                                *sum = crate::sql::expr::numeric::checked_decimal_add(*sum, *d)?;
                            }
                        }
                        Value::Vector(v) => {
                            if let Some(sv) = sum_vector.as_mut() {
                                if sv.len() != v.len() {
                                    return Err(anyhow!(
                                        "cannot average vectors of different dimensions"
                                    ));
                                }
                                for (a, b) in sv.iter_mut().zip(v.iter()) {
                                    *a += b;
                                }
                            } else {
                                *sum_vector = Some(v.clone());
                            }
                        }
                        _ => return Err(anyhow!("AVG requires numeric type")),
                    }
                    *count += 1;
                    let after = sum_vector.as_deref().map(vector_state_size).unwrap_or(0);
                    let delta = AggregateStateDelta::from_sizes(before, after);
                    if delta.grow_bytes > 0 {
                        charge(delta.grow_bytes)?;
                    }
                    delta
                } else {
                    AggregateStateDelta::none()
                }
            }
            Aggregator::StringAgg { entries } => {
                if !matches!(val, Value::Null) {
                    let (payload_len, prebuilt) = string_agg_payload(val);
                    let retained_bytes = payload_len.saturating_add(1);
                    let (admitted, additional) =
                        admit_accumulating_push(entries, retained_bytes, charge)?;
                    if additional > 0 {
                        entries.reserve_exact(additional);
                    }
                    let s = match prebuilt {
                        Some(s) => s,
                        None => {
                            let Value::Text(s) = val else {
                                return Err(anyhow!("string_agg input changed type mid-update"));
                            };
                            s.clone()
                        }
                    };
                    // When called via generic update() (no per-row delimiter),
                    // use "," as the fallback. The per-row path goes through
                    // update_string_agg() instead.
                    entries.push((s, ",".to_string()));
                    AggregateStateDelta::grow(admitted)
                } else {
                    AggregateStateDelta::none()
                }
            }
            Aggregator::ArrayAgg { values } => {
                let (admitted, additional) =
                    admit_accumulating_push(values, estimate_value_size(val), charge)?;
                if additional > 0 {
                    values.reserve_exact(additional);
                }
                values.push(val.clone());
                AggregateStateDelta::grow(admitted)
            }
            Aggregator::BoolAnd(current) => match val {
                Value::Null => AggregateStateDelta::none(),
                Value::Boolean(b) => {
                    *current = Some(current.unwrap_or(true) && *b);
                    AggregateStateDelta::none()
                }
                _ => return Err(anyhow!("BOOL_AND requires boolean type")),
            },
            Aggregator::BoolOr(current) => match val {
                Value::Null => AggregateStateDelta::none(),
                Value::Boolean(b) => {
                    *current = Some(current.unwrap_or(false) || *b);
                    AggregateStateDelta::none()
                }
                _ => return Err(anyhow!("BOOL_OR requires boolean type")),
            },
            Aggregator::JsonAgg { values } | Aggregator::JsonbAgg { values } => {
                let (admitted, additional) =
                    admit_accumulating_push(values, estimate_value_size(val), charge)?;
                if additional > 0 {
                    values.reserve_exact(additional);
                }
                values.push(val.clone());
                AggregateStateDelta::grow(admitted)
            }
        };
        Ok(delta)
    }

    pub fn result(&self) -> Result<Value> {
        Ok(match self {
            Aggregator::Count(c) => Value::Int64(*c),
            Aggregator::Sum { value, .. } => value.clone(),
            Aggregator::Max(v) => v.clone(),
            Aggregator::Min(v) => v.clone(),
            Aggregator::Avg {
                sum,
                sum_float,
                sum_vector,
                count,
            } => {
                if *count == 0 {
                    Value::Null
                } else if let Some(sv) = sum_vector {
                    vector_value(sv.iter().map(|x| x / *count as f64).collect())?
                } else if let Some(sf) = sum_float {
                    Value::Float64(*sf / *count as f64)
                } else {
                    let denom = Decimal::from(*count);
                    Value::Numeric(pg_numeric_div(*sum, denom)?)
                }
            }
            Aggregator::StringAgg { entries } => {
                if entries.is_empty() {
                    Value::Null
                } else {
                    // PostgreSQL semantics: for i > 0, delimiter[i] is placed
                    // before value[i] (between value[i-1] and value[i]).
                    let mut result = entries[0].0.clone();
                    for (val, delim) in &entries[1..] {
                        result.push_str(delim);
                        result.push_str(val);
                    }
                    Value::Text(result)
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
        })
    }

    /// Finalize by consuming retained state, admitting state-sized output
    /// allocations through `charge` **before** they are materialized.
    ///
    /// `result()` duplicates the whole retained state for accumulating
    /// aggregates (string join, values clone, JSON build), which reopens the
    /// #2555 OOM shape after every input row was admitted. Here:
    ///
    /// - `array_agg` moves its values out — no copy, nothing new to charge.
    /// - `string_agg` charges the exact output length before allocating it.
    ///   The length is arithmetic over the retained strings' real `len()`s —
    ///   a measurement of existing objects, never a prediction of formatter
    ///   behavior.
    /// - `json/jsonb_agg` charge each item string after it is built (one
    ///   O(item) in-flight transient, within contract), then charge the exact
    ///   output length before assembling it in a single allocation.
    /// - Fixed-size aggregators delegate to `result()`.
    ///
    /// Retained state consumed here stays charged by the caller until its
    /// group ledger is released, so accounting remains conservative while the
    /// state and the output briefly coexist.
    pub fn result_consuming(
        &mut self,
        charge: &mut dyn FnMut(usize) -> Result<()>,
    ) -> Result<Value> {
        match self {
            Aggregator::StringAgg { entries } => {
                if entries.is_empty() {
                    return Ok(Value::Null);
                }
                let entries = std::mem::take(entries);
                let total: usize = entries[0].0.len()
                    + entries[1..]
                        .iter()
                        .map(|(val, delim)| delim.len() + val.len())
                        .sum::<usize>();
                charge(total)?;
                let mut result = String::with_capacity(total);
                // PostgreSQL semantics: for i > 0, delimiter[i] is placed
                // before value[i] (between value[i-1] and value[i]).
                for (i, (val, delim)) in entries.iter().enumerate() {
                    if i > 0 {
                        result.push_str(delim);
                    }
                    result.push_str(val);
                }
                Ok(Value::Text(result))
            }
            Aggregator::ArrayAgg { values } => {
                if values.is_empty() {
                    Ok(Value::Null)
                } else {
                    Ok(Value::Array(std::mem::take(values)))
                }
            }
            Aggregator::JsonAgg { values } => {
                if values.is_empty() {
                    Ok(Value::Null)
                } else {
                    Self::json_agg_result_consuming(values, charge, true).map(Value::Json)
                }
            }
            Aggregator::JsonbAgg { values } => {
                if values.is_empty() {
                    Ok(Value::Null)
                } else {
                    Self::json_agg_result_consuming(values, charge, false).map(Value::Jsonb)
                }
            }
            other => other.result(),
        }
    }

    fn json_agg_result_consuming(
        values: &mut Vec<Value>,
        charge: &mut dyn FnMut(usize) -> Result<()>,
        canonicalize_jsonb: bool,
    ) -> Result<String> {
        let values = std::mem::take(values);
        // Item Vec slots are sized from the real element count.
        charge(values.len().saturating_mul(std::mem::size_of::<String>()))?;
        let mut items = Vec::with_capacity(values.len());
        let mut items_bytes = 0usize;
        for v in &values {
            let item = value_to_json_str_inner(v, canonicalize_jsonb);
            charge(item.len())?;
            items_bytes = items_bytes.saturating_add(item.len());
            items.push(item);
        }
        drop(values);
        let total = 2 + items_bytes + items.len().saturating_sub(1);
        charge(total)?;
        let mut out = String::with_capacity(total);
        out.push('[');
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(item);
        }
        out.push(']');
        Ok(out)
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
        (Value::Int32(l), Value::Int32(r)) => {
            l.checked_add(*r).map(Value::Int32).ok_or_else(|| {
                SqlError::NumericValueOutOfRange {
                    message: "integer out of range".into(),
                }
                .into()
            })
        }
        (Value::Int64(l), Value::Int64(r)) => {
            l.checked_add(*r).map(Value::Int64).ok_or_else(|| {
                SqlError::NumericValueOutOfRange {
                    message: "bigint out of range".into(),
                }
                .into()
            })
        }
        (Value::Int32(l), Value::Int64(r)) => (*l as i64)
            .checked_add(*r)
            .map(Value::Int64)
            .ok_or_else(|| {
                SqlError::NumericValueOutOfRange {
                    message: "bigint out of range".into(),
                }
                .into()
            }),
        (Value::Int64(l), Value::Int32(r)) => {
            l.checked_add(*r as i64).map(Value::Int64).ok_or_else(|| {
                SqlError::NumericValueOutOfRange {
                    message: "bigint out of range".into(),
                }
                .into()
            })
        }
        (Value::Float64(l), Value::Float64(r)) => Ok(Value::Float64(l + r)),
        (Value::Int32(l), Value::Float64(r)) => Ok(Value::Float64(*l as f64 + r)),
        (Value::Float64(l), Value::Int32(r)) => Ok(Value::Float64(l + *r as f64)),
        (Value::Int64(l), Value::Float64(r)) => Ok(Value::Float64(*l as f64 + r)),
        (Value::Float64(l), Value::Int64(r)) => Ok(Value::Float64(l + *r as f64)),
        // Handle Text values that can be parsed as numbers (PostgreSQL behavior)
        (Value::Text(l), Value::Text(r)) => {
            // Try parsing as int first, then float
            match (l.parse::<i64>(), r.parse::<i64>()) {
                (Ok(li), Ok(ri)) => li.checked_add(ri).map(Value::Int64).ok_or_else(|| {
                    SqlError::NumericValueOutOfRange {
                        message: "bigint out of range".into(),
                    }
                    .into()
                }),
                _ => match (l.parse::<f64>(), r.parse::<f64>()) {
                    (Ok(lf), Ok(rf)) => Ok(Value::Float64(lf + rf)),
                    _ => Err(anyhow!("Cannot add non-numeric text values")),
                },
            }
        }
        (Value::Text(t), Value::Int32(i)) | (Value::Int32(i), Value::Text(t)) => {
            if let Ok(ti) = t.parse::<i32>() {
                ti.checked_add(*i).map(Value::Int32).ok_or_else(|| {
                    SqlError::NumericValueOutOfRange {
                        message: "integer out of range".into(),
                    }
                    .into()
                })
            } else if let Ok(tf) = t.parse::<f64>() {
                Ok(Value::Float64(tf + *i as f64))
            } else {
                Err(anyhow!("Cannot add non-numeric text to number"))
            }
        }
        (Value::Text(t), Value::Int64(i)) | (Value::Int64(i), Value::Text(t)) => {
            if let Ok(ti) = t.parse::<i64>() {
                ti.checked_add(*i).map(Value::Int64).ok_or_else(|| {
                    SqlError::NumericValueOutOfRange {
                        message: "bigint out of range".into(),
                    }
                    .into()
                })
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
        (Value::Numeric(l), Value::Numeric(r)) => Ok(Value::Numeric(
            crate::sql::expr::numeric::checked_decimal_add(*l, *r)?,
        )),
        (Value::Numeric(d), Value::Int32(i)) | (Value::Int32(i), Value::Numeric(d)) => {
            Ok(Value::Numeric(
                crate::sql::expr::numeric::checked_decimal_add(*d, Decimal::from(*i))?,
            ))
        }
        (Value::Numeric(d), Value::Int64(i)) | (Value::Int64(i), Value::Numeric(d)) => {
            Ok(Value::Numeric(
                crate::sql::expr::numeric::checked_decimal_add(*d, Decimal::from(*i))?,
            ))
        }
        (Value::Numeric(d), Value::Float64(f)) | (Value::Float64(f), Value::Numeric(d)) => {
            let df = d
                .to_f64()
                .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
            Ok(Value::Float64(df + *f))
        }
        (Value::Vector(l), Value::Vector(r)) => {
            if l.len() != r.len() {
                return Err(anyhow!("cannot add vectors of different dimensions"));
            }
            vector_value(l.iter().zip(r.iter()).map(|(a, b)| a + b).collect())
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
        assert_eq!(agg.result().unwrap(), Value::Int64(3));
    }

    #[test]
    fn test_count_empty() {
        let agg = Aggregator::new("COUNT", None).unwrap();
        assert_eq!(agg.result().unwrap(), Value::Int64(0));
    }

    #[test]
    fn test_sum_int32() {
        // Default return type is Numeric; Int32 values are widened
        let mut agg = Aggregator::new("SUM", None).unwrap();
        agg.update(&Value::Int32(10)).unwrap();
        agg.update(&Value::Int32(20)).unwrap();
        agg.update(&Value::Int32(30)).unwrap();
        assert_eq!(agg.result().unwrap(), Value::Numeric(Decimal::from(60)));
    }

    #[test]
    fn test_sum_with_null() {
        let mut agg = Aggregator::new("SUM", None).unwrap();
        agg.update(&Value::Int32(10)).unwrap();
        agg.update(&Value::Null).unwrap();
        agg.update(&Value::Int32(20)).unwrap();
        assert_eq!(agg.result().unwrap(), Value::Numeric(Decimal::from(30)));
    }

    #[test]
    fn test_sum_empty() {
        let agg = Aggregator::new("SUM", None).unwrap();
        assert_eq!(agg.result().unwrap(), Value::Null);
    }

    #[test]
    fn test_sum_vector() {
        let mut agg = Aggregator::new("SUM", None).unwrap();
        agg.update(&Value::Vector(vec![1.0, 2.0, 3.0])).unwrap();
        agg.update(&Value::Vector(vec![4.0, 5.0, 6.0])).unwrap();
        agg.update(&Value::Vector(vec![7.0, 8.0, 9.0])).unwrap();
        assert_eq!(agg.result().unwrap(), Value::Vector(vec![12.0, 15.0, 18.0]));
    }

    #[test]
    fn test_sum_vector_with_null() {
        let mut agg = Aggregator::new("SUM", None).unwrap();
        agg.update(&Value::Vector(vec![1.0, 2.0, 3.0])).unwrap();
        agg.update(&Value::Null).unwrap();
        agg.update(&Value::Vector(vec![4.0, 5.0, 6.0])).unwrap();
        assert_eq!(agg.result().unwrap(), Value::Vector(vec![5.0, 7.0, 9.0]));
    }

    #[test]
    fn test_max() {
        let mut agg = Aggregator::new("MAX", None).unwrap();
        agg.update(&Value::Int32(5)).unwrap();
        agg.update(&Value::Int32(10)).unwrap();
        agg.update(&Value::Int32(3)).unwrap();
        assert_eq!(agg.result().unwrap(), Value::Int32(10));
    }

    #[test]
    fn test_max_with_null() {
        let mut agg = Aggregator::new("MAX", None).unwrap();
        agg.update(&Value::Null).unwrap();
        agg.update(&Value::Int32(5)).unwrap();
        agg.update(&Value::Null).unwrap();
        assert_eq!(agg.result().unwrap(), Value::Int32(5));
    }

    #[test]
    fn test_min() {
        let mut agg = Aggregator::new("MIN", None).unwrap();
        agg.update(&Value::Int32(5)).unwrap();
        agg.update(&Value::Int32(2)).unwrap();
        agg.update(&Value::Int32(8)).unwrap();
        assert_eq!(agg.result().unwrap(), Value::Int32(2));
    }

    #[test]
    fn test_avg() {
        let mut agg = Aggregator::new("AVG", None).unwrap();
        agg.update(&Value::Int32(10)).unwrap();
        agg.update(&Value::Int32(20)).unwrap();
        agg.update(&Value::Int32(30)).unwrap();
        let result = agg.result().unwrap();
        assert_eq!(result, Value::Numeric(Decimal::from(20)));
    }

    #[test]
    fn test_avg_with_null() {
        let mut agg = Aggregator::new("AVG", None).unwrap();
        agg.update(&Value::Int32(10)).unwrap();
        agg.update(&Value::Null).unwrap();
        agg.update(&Value::Int32(20)).unwrap();
        let result = agg.result().unwrap();
        assert_eq!(result, Value::Numeric(Decimal::from(15)));
    }

    #[test]
    fn test_avg_empty() {
        let agg = Aggregator::new("AVG", None).unwrap();
        assert_eq!(agg.result().unwrap(), Value::Null);
    }

    #[test]
    fn test_avg_vector() {
        let mut agg = Aggregator::new("AVG", None).unwrap();
        agg.update(&Value::Vector(vec![1.0, 2.0, 3.0])).unwrap();
        agg.update(&Value::Vector(vec![4.0, 5.0, 6.0])).unwrap();
        agg.update(&Value::Vector(vec![7.0, 8.0, 9.0])).unwrap();
        assert_eq!(agg.result().unwrap(), Value::Vector(vec![4.0, 5.0, 6.0]));
    }

    #[test]
    fn test_avg_vector_with_null() {
        let mut agg = Aggregator::new("AVG", None).unwrap();
        agg.update(&Value::Vector(vec![1.0, 2.0, 3.0])).unwrap();
        agg.update(&Value::Null).unwrap();
        agg.update(&Value::Vector(vec![7.0, 8.0, 9.0])).unwrap();
        assert_eq!(agg.result().unwrap(), Value::Vector(vec![4.0, 5.0, 6.0]));
    }

    #[test]
    fn test_avg_vector_empty() {
        let agg = Aggregator::new("AVG", None).unwrap();
        assert_eq!(agg.result().unwrap(), Value::Null);
    }

    #[test]
    fn test_avg_float() {
        let mut agg = Aggregator::new("AVG", None).unwrap();
        agg.update(&Value::Float64(1.5)).unwrap();
        agg.update(&Value::Float64(2.5)).unwrap();
        let result = agg.result().unwrap();
        assert_eq!(result, Value::Float64(2.0));
    }

    #[test]
    fn test_avg_int_repeating_precision() {
        let mut agg = Aggregator::new("AVG", None).unwrap();
        agg.update(&Value::Int32(300)).unwrap();
        agg.update(&Value::Int32(200)).unwrap();
        agg.update(&Value::Int32(300)).unwrap();
        let result = agg.result().unwrap();
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
        assert_eq!(agg.result().unwrap(), Value::Text("cherry".to_string()));
    }

    #[test]
    fn test_min_text() {
        let mut agg = Aggregator::new("MIN", None).unwrap();
        agg.update(&Value::Text("banana".to_string())).unwrap();
        agg.update(&Value::Text("apple".to_string())).unwrap();
        agg.update(&Value::Text("cherry".to_string())).unwrap();
        assert_eq!(agg.result().unwrap(), Value::Text("apple".to_string()));
    }

    #[test]
    fn test_string_agg() {
        let mut agg = Aggregator::new_string_agg();
        agg.update_string_agg(&Value::Text("apple".to_string()), ", ")
            .unwrap();
        agg.update_string_agg(&Value::Text("banana".to_string()), ", ")
            .unwrap();
        agg.update_string_agg(&Value::Text("cherry".to_string()), ", ")
            .unwrap();
        assert_eq!(
            agg.result().unwrap(),
            Value::Text("apple, banana, cherry".to_string())
        );
    }

    #[test]
    fn test_string_agg_with_null() {
        let mut agg = Aggregator::new_string_agg();
        agg.update_string_agg(&Value::Text("a".to_string()), ",")
            .unwrap();
        agg.update_string_agg(&Value::Null, ",").unwrap();
        agg.update_string_agg(&Value::Text("b".to_string()), ",")
            .unwrap();
        assert_eq!(agg.result().unwrap(), Value::Text("a,b".to_string()));
    }

    #[test]
    fn test_string_agg_empty() {
        let agg = Aggregator::new_string_agg();
        assert_eq!(agg.result().unwrap(), Value::Null);
    }

    #[test]
    fn test_string_agg_varying_delimiters() {
        // PostgreSQL: delimiter from row *i* is placed between value[i] and value[i+1]
        let mut agg = Aggregator::new_string_agg();
        agg.update_string_agg(&Value::Text("a".to_string()), ",")
            .unwrap();
        agg.update_string_agg(&Value::Text("b".to_string()), ";")
            .unwrap();
        agg.update_string_agg(&Value::Text("c".to_string()), "|")
            .unwrap();
        // delimiter[1]=; goes between a and b, delimiter[2]=| goes between b and c
        assert_eq!(agg.result().unwrap(), Value::Text("a;b|c".to_string()));
    }

    #[test]
    fn test_array_agg() {
        let mut agg = Aggregator::new("ARRAY_AGG", None).unwrap();
        agg.update(&Value::Int32(1)).unwrap();
        agg.update(&Value::Int32(2)).unwrap();
        agg.update(&Value::Int32(3)).unwrap();
        assert_eq!(
            agg.result().unwrap(),
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
            agg.result().unwrap(),
            Value::Array(vec![Value::Int32(1), Value::Null, Value::Int32(2)])
        );
    }

    #[test]
    fn test_array_agg_empty() {
        let agg = Aggregator::new("ARRAY_AGG", None).unwrap();
        assert_eq!(agg.result().unwrap(), Value::Null);
    }

    #[test]
    fn test_jsonb_agg_returns_jsonb_type() {
        let mut agg = Aggregator::new("JSONB_AGG", None).unwrap();
        agg.update(&Value::Int32(1)).unwrap();
        agg.update(&Value::Int32(2)).unwrap();
        agg.update(&Value::Int32(3)).unwrap();
        match agg.result().unwrap() {
            Value::Jsonb(s) => assert_eq!(s, "[1,2,3]"),
            other => panic!("expected Value::Jsonb, got {:?}", other),
        }
    }

    #[test]
    fn test_jsonb_agg_with_jsonb_inputs() {
        let mut agg = Aggregator::new("JSONB_AGG", None).unwrap();
        agg.update(&Value::Jsonb(r#"{"a":1}"#.to_string())).unwrap();
        agg.update(&Value::Jsonb(r#"{"b":2}"#.to_string())).unwrap();
        match agg.result().unwrap() {
            Value::Jsonb(s) => assert_eq!(s, r#"[{"a":1},{"b":2}]"#),
            other => panic!("expected Value::Jsonb, got {:?}", other),
        }
    }

    #[test]
    fn test_jsonb_agg_empty() {
        let agg = Aggregator::new("JSONB_AGG", None).unwrap();
        assert_eq!(agg.result().unwrap(), Value::Null);
    }

    #[test]
    fn test_json_agg_returns_json_type() {
        let mut agg = Aggregator::new("JSON_AGG", None).unwrap();
        agg.update(&Value::Int32(1)).unwrap();
        agg.update(&Value::Int32(2)).unwrap();
        match agg.result().unwrap() {
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
        match agg.result().unwrap() {
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
        match agg.result().unwrap() {
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
        assert_eq!(agg.result().unwrap(), Value::Int64(60));
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
        assert_eq!(agg.result().unwrap(), Value::Numeric(Decimal::from(300)));
    }

    // Retained-memory delta contract (#2555): for aggregators whose state
    // grows with input, cumulative `grow_bytes` must cover the retained
    // payload (conservative, never structurally under-counting); fixed-size
    // aggregators must not report growth proportional to input count.

    #[test]
    fn test_state_delta_count_reports_no_growth() {
        let mut agg = Aggregator::new("COUNT", None).unwrap();
        for i in 0..100 {
            let delta = agg.update(&Value::Int32(i)).unwrap();
            assert_eq!(delta, AggregateStateDelta::default());
        }
    }

    #[test]
    fn test_state_delta_null_input_reports_no_growth() {
        let mut sum = Aggregator::new("SUM", None).unwrap();
        assert_eq!(
            sum.update(&Value::Null).unwrap(),
            AggregateStateDelta::default()
        );
        let mut string_agg = Aggregator::new_string_agg();
        assert_eq!(
            string_agg.update_string_agg(&Value::Null, ",").unwrap(),
            AggregateStateDelta::default()
        );
    }

    #[test]
    fn test_state_delta_string_agg_covers_retained_payload() {
        let mut agg = Aggregator::new_string_agg();
        let mut charged = 0usize;
        let mut payload = 0usize;
        for i in 0..50 {
            let s = format!("value-{i:04}-{}", "x".repeat(64));
            let delim = "; ";
            let delta = agg
                .update_string_agg(&Value::Text(s.clone()), delim)
                .unwrap();
            assert_eq!(delta.shrink_bytes, 0);
            charged += delta.grow_bytes;
            payload += s.len() + delim.len();
        }
        assert!(
            charged >= payload,
            "string_agg charged {charged} bytes for {payload} retained payload bytes"
        );
    }

    #[test]
    fn test_state_delta_array_agg_covers_retained_values() {
        let mut agg = Aggregator::new("ARRAY_AGG", None).unwrap();
        let mut charged = 0usize;
        let mut payload = 0usize;
        for i in 0..50 {
            let val = Value::Text(format!("row-{i}-{}", "y".repeat(128)));
            payload += estimate_value_size(&val);
            let delta = agg.update(&val).unwrap();
            charged += delta.grow_bytes;
        }
        assert!(
            charged >= payload,
            "array_agg charged {charged} bytes for {payload} retained value bytes"
        );
    }

    #[test]
    fn test_state_delta_json_agg_covers_retained_values() {
        let mut agg = Aggregator::new("JSON_AGG", None).unwrap();
        let val = Value::Text("z".repeat(1024));
        let delta = agg.update(&val).unwrap();
        assert!(
            delta.grow_bytes >= estimate_value_size(&val),
            "json_agg delta {delta:?} must cover the cloned value"
        );
    }

    #[test]
    fn test_state_delta_min_replacement_reports_shrink() {
        let mut agg = Aggregator::new("MIN", None).unwrap();
        let wide = Value::Text("m".repeat(512));
        let grow = agg.update(&wide).unwrap();
        assert!(grow.grow_bytes >= 512);
        // A lexically-smaller, shorter value replaces the wide one.
        let shrink = agg.update(&Value::Text("a".to_string())).unwrap();
        assert!(
            shrink.shrink_bytes > 0,
            "replacing retained MIN state with a smaller value should shrink, got {shrink:?}"
        );
    }

    #[test]
    fn test_state_delta_sum_not_proportional_to_input_count() {
        let mut agg = Aggregator::new("SUM", None).unwrap();
        let mut charged = 0usize;
        for i in 0..1000 {
            charged += agg.update(&Value::Int32(i)).unwrap().grow_bytes;
        }
        // SUM retains one numeric value; growth must stay bounded by the
        // state size, not the number of input rows.
        assert!(
            charged < 1024,
            "SUM charged {charged} bytes over 1000 updates; state is fixed-size"
        );
    }

    fn accumulating_len(agg: &Aggregator) -> (usize, usize) {
        match agg {
            Aggregator::ArrayAgg { values }
            | Aggregator::JsonAgg { values }
            | Aggregator::JsonbAgg { values } => (values.len(), values.capacity()),
            Aggregator::StringAgg { entries } => (entries.len(), entries.capacity()),
            _ => unreachable!("not an accumulating aggregator"),
        }
    }

    #[test]
    fn test_update_charged_admits_growth_before_allocation() {
        // Every accumulating arm must follow the same shape: a rejected
        // charge aborts before any allocation or state change (a full Vec's
        // doubling is state-sized and needs admission first). Testing one
        // arm of an N-arm pattern lets the siblings drift.
        for kind in ["ARRAY_AGG", "JSON_AGG", "JSONB_AGG", "STRING_AGG"] {
            let mut agg = Aggregator::new(kind, None).unwrap();
            // Fill exactly to capacity so the next push needs slot growth.
            for i in 0..4 {
                agg.update(&Value::Int32(i)).unwrap();
            }
            let (len_before, cap_before) = accumulating_len(&agg);
            assert_eq!((len_before, cap_before), (4, 4), "{kind} setup");

            let err = agg.update_charged(&Value::Int32(99), &mut |_| Err(anyhow!("quota")));
            assert!(err.is_err(), "{kind} must propagate the rejection");
            let (len, cap) = accumulating_len(&agg);
            assert_eq!(len, 4, "{kind}: rejected update must not retain");
            assert_eq!(cap, 4, "{kind}: rejected growth must not allocate");

            // A successful charge admits payload plus the explicit slot
            // growth before the push.
            let mut charged = 0usize;
            agg.update_charged(&Value::Int32(99), &mut |b| {
                charged += b;
                Ok(())
            })
            .unwrap();
            let (len, cap) = accumulating_len(&agg);
            assert_eq!((len, cap), (5, 8), "{kind} growth shape");
            assert!(charged > 0, "{kind} must admit through the callback");
        }
    }

    #[test]
    fn test_string_agg_charged_admits_text_before_clone() {
        let mut agg = Aggregator::new_string_agg();
        let wide = Value::Text("w".repeat(1 << 20));
        // The admitted amount must be known from the borrowed value: exactly
        // payload + delimiter + initial slots, charged before the retained
        // clone is built.
        let mut charged = 0usize;
        agg.update_string_agg_charged(&wide, "; ", &mut |b| {
            charged += b;
            Ok(())
        })
        .unwrap();
        let expected_payload = (1 << 20) + 2;
        assert!(
            charged >= expected_payload,
            "charge {charged} must cover the {expected_payload}-byte retained payload"
        );
        // Rejection leaves no retained entry.
        let err = agg.update_string_agg_charged(&wide, "; ", &mut |_| Err(anyhow!("quota")));
        assert!(err.is_err());
        let Aggregator::StringAgg { entries } = &agg else {
            unreachable!()
        };
        assert_eq!(entries.len(), 1);
    }

    // Consuming finalization contract (#2555 / #2612 review): state-sized
    // output allocations must be admitted via the charge callback before they
    // are materialized, and the produced values must match `result()`.

    fn twin_aggregators(kind: &str, inputs: &[Value]) -> (Aggregator, Aggregator) {
        let mut a = Aggregator::new(kind, None).unwrap();
        let mut b = Aggregator::new(kind, None).unwrap();
        for v in inputs {
            a.update(v).unwrap();
            b.update(v).unwrap();
        }
        (a, b)
    }

    fn charged_result(agg: &mut Aggregator) -> (Value, usize) {
        let mut charged = 0usize;
        let value = agg
            .result_consuming(&mut |bytes| {
                charged += bytes;
                Ok(())
            })
            .unwrap();
        (value, charged)
    }

    #[test]
    fn test_result_consuming_string_agg_precharges_exact_output() {
        let mut reference = Aggregator::new_string_agg();
        let mut consuming = Aggregator::new_string_agg();
        for i in 0..20 {
            let v = Value::Text(format!("item-{i}-{}", "x".repeat(32)));
            reference.update_string_agg(&v, "; ").unwrap();
            consuming.update_string_agg(&v, "; ").unwrap();
        }
        let expected = reference.result().unwrap();
        let (value, charged) = charged_result(&mut consuming);
        assert_eq!(value, expected);
        let Value::Text(s) = &value else {
            panic!("string_agg must produce text");
        };
        assert_eq!(charged, s.len(), "charge must equal the real output size");
    }

    #[test]
    fn test_result_consuming_array_agg_moves_without_charge() {
        let inputs: Vec<Value> = (0..10).map(|i| Value::Text(format!("v{i}"))).collect();
        let (reference, mut consuming) = twin_aggregators("ARRAY_AGG", &inputs);
        let expected = reference.result().unwrap();
        let (value, charged) = charged_result(&mut consuming);
        assert_eq!(value, expected);
        assert_eq!(charged, 0, "moving retained values must not allocate");
    }

    #[test]
    fn test_result_consuming_json_agg_matches_and_covers_output() {
        for kind in ["JSON_AGG", "JSONB_AGG"] {
            let inputs: Vec<Value> = (0..10)
                .map(|i| Value::Text(format!("needs \"escaping\" {i}")))
                .collect();
            let (reference, mut consuming) = twin_aggregators(kind, &inputs);
            let expected = reference.result().unwrap();
            let (value, charged) = charged_result(&mut consuming);
            assert_eq!(
                value, expected,
                "{kind} consuming output must match result()"
            );
            let out_len = match &value {
                Value::Json(s) | Value::Jsonb(s) => s.len(),
                other => panic!("{kind} produced {other:?}"),
            };
            assert!(
                charged >= out_len,
                "{kind} charged {charged} bytes for {out_len} output bytes"
            );
        }
    }

    #[test]
    fn test_result_consuming_empty_accumulators_return_null_without_charge() {
        for kind in ["STRING_AGG", "ARRAY_AGG", "JSON_AGG", "JSONB_AGG"] {
            let mut agg = Aggregator::new(kind, None).unwrap();
            let (value, charged) = charged_result(&mut agg);
            assert_eq!(value, Value::Null, "{kind} over empty input");
            assert_eq!(charged, 0, "{kind} over empty input must charge nothing");
        }
    }

    #[test]
    fn test_result_consuming_charge_failure_aborts_finalization() {
        let mut agg = Aggregator::new_string_agg();
        agg.update_string_agg(&Value::Text("payload".to_string()), ",")
            .unwrap();
        let err = agg.result_consuming(&mut |_| Err(anyhow!("quota exceeded")));
        assert!(err.is_err(), "charge rejection must abort finalization");
    }

    #[test]
    fn test_result_consuming_fixed_size_aggregators_delegate() {
        let inputs: Vec<Value> = (1..=5).map(Value::Int32).collect();
        for kind in ["COUNT", "SUM", "MAX", "MIN", "AVG"] {
            let (reference, mut consuming) = twin_aggregators(kind, &inputs);
            let expected = reference.result().unwrap();
            let (value, charged) = charged_result(&mut consuming);
            assert_eq!(
                value, expected,
                "{kind} consuming output must match result()"
            );
            assert_eq!(charged, 0, "{kind} state is fixed-size; nothing to charge");
        }
    }
}
