use anyhow::{anyhow, Result};
use arrow_array::{
    Array, BinaryArray, BooleanArray, Date32Array, Date64Array, Decimal128Array,
    FixedSizeBinaryArray, Float16Array, Float32Array, Float64Array, Int16Array, Int32Array,
    Int64Array, Int8Array, LargeBinaryArray, LargeListArray, LargeStringArray, ListArray, MapArray,
    StringArray, StructArray, TimestampMicrosecondArray, TimestampMillisecondArray,
    TimestampNanosecondArray, TimestampSecondArray, UInt16Array, UInt32Array, UInt64Array,
    UInt8Array,
};
use arrow_cast::cast;
use arrow_schema::{DataType as ArrowDataType, TimeUnit};
use rust_decimal::Decimal;
use serde_json::{Map as JsonMap, Number as JsonNumber, Value as JsonValue};

use crate::types::{DataType, Value};

const MILLIS_PER_DAY: i64 = 86_400_000;

pub(crate) fn arrow_type_to_pg_type(arrow_type: &ArrowDataType) -> Result<DataType> {
    match arrow_type {
        ArrowDataType::Boolean => Ok(DataType::Boolean),
        ArrowDataType::Int8 | ArrowDataType::Int16 | ArrowDataType::Int32 => Ok(DataType::Int32),
        ArrowDataType::Int64 => Ok(DataType::Int64),
        ArrowDataType::UInt8 | ArrowDataType::UInt16 => Ok(DataType::Int32),
        ArrowDataType::UInt32 => Ok(DataType::Int64),
        ArrowDataType::UInt64 => Ok(DataType::Numeric {
            precision: None,
            scale: None,
        }),
        ArrowDataType::Float16 | ArrowDataType::Float32 | ArrowDataType::Float64 => {
            Ok(DataType::Float64)
        }
        ArrowDataType::Utf8 | ArrowDataType::LargeUtf8 => Ok(DataType::Text),
        ArrowDataType::Binary | ArrowDataType::LargeBinary | ArrowDataType::FixedSizeBinary(_) => {
            Ok(DataType::Bytes)
        }
        ArrowDataType::Date32 | ArrowDataType::Date64 => Ok(DataType::Date),
        ArrowDataType::Timestamp(_, _) => Ok(DataType::Timestamp),
        ArrowDataType::Decimal128(precision, scale) => {
            if *precision > 28 {
                return Err(anyhow!(
                    "Parquet Decimal128 with precision {} exceeds pg-tikv's maximum precision of 28",
                    precision
                ));
            }
            Ok(DataType::Numeric {
                precision: Some((*precision).into()),
                scale: if *scale >= 0 {
                    Some(*scale as u32)
                } else {
                    None
                },
            })
        }
        ArrowDataType::Dictionary(_, value_type) => arrow_type_to_pg_type(value_type),
        ArrowDataType::List(_) | ArrowDataType::LargeList(_) | ArrowDataType::Struct(_) => {
            Ok(DataType::Jsonb)
        }
        ArrowDataType::Map(_, _) => Ok(DataType::Jsonb),
        ArrowDataType::Null => Ok(DataType::Text),
        _ => Err(anyhow!("Unsupported Arrow type: {:?}", arrow_type)),
    }
}

pub(crate) fn arrow_array_to_value(array: &dyn Array, row_idx: usize) -> Result<Value> {
    if row_idx >= array.len() {
        return Err(anyhow!(
            "Row index {} out of bounds for array length {}",
            row_idx,
            array.len()
        ));
    }

    if array.is_null(row_idx) {
        return Ok(Value::Null);
    }

    match array.data_type() {
        ArrowDataType::Boolean => Ok(Value::Boolean(
            downcast_array::<BooleanArray>(array)?.value(row_idx),
        )),
        ArrowDataType::Int8 => Ok(Value::Int32(
            downcast_array::<Int8Array>(array)?.value(row_idx) as i32,
        )),
        ArrowDataType::Int16 => Ok(Value::Int32(
            downcast_array::<Int16Array>(array)?.value(row_idx) as i32,
        )),
        ArrowDataType::Int32 => Ok(Value::Int32(
            downcast_array::<Int32Array>(array)?.value(row_idx),
        )),
        ArrowDataType::Int64 => Ok(Value::Int64(
            downcast_array::<Int64Array>(array)?.value(row_idx),
        )),
        ArrowDataType::UInt8 => Ok(Value::Int32(
            downcast_array::<UInt8Array>(array)?.value(row_idx) as i32,
        )),
        ArrowDataType::UInt16 => Ok(Value::Int32(
            downcast_array::<UInt16Array>(array)?.value(row_idx) as i32,
        )),
        ArrowDataType::UInt32 => Ok(Value::Int64(
            downcast_array::<UInt32Array>(array)?.value(row_idx) as i64,
        )),
        ArrowDataType::UInt64 => {
            let v = downcast_array::<UInt64Array>(array)?.value(row_idx);
            Ok(Value::Numeric(Decimal::try_from_i128_with_scale(
                i128::from(v),
                0,
            )?))
        }
        ArrowDataType::Float16 => {
            let v = downcast_array::<Float16Array>(array)?.value(row_idx);
            Ok(Value::Float64(f64::from(f32::from(v))))
        }
        ArrowDataType::Float32 => Ok(Value::Float64(
            downcast_array::<Float32Array>(array)?.value(row_idx) as f64,
        )),
        ArrowDataType::Float64 => Ok(Value::Float64(
            downcast_array::<Float64Array>(array)?.value(row_idx),
        )),
        ArrowDataType::Utf8 => Ok(Value::Text(
            downcast_array::<StringArray>(array)?
                .value(row_idx)
                .to_string(),
        )),
        ArrowDataType::LargeUtf8 => Ok(Value::Text(
            downcast_array::<LargeStringArray>(array)?
                .value(row_idx)
                .to_string(),
        )),
        ArrowDataType::Binary => Ok(Value::Bytes(
            downcast_array::<BinaryArray>(array)?
                .value(row_idx)
                .to_vec(),
        )),
        ArrowDataType::LargeBinary => Ok(Value::Bytes(
            downcast_array::<LargeBinaryArray>(array)?
                .value(row_idx)
                .to_vec(),
        )),
        ArrowDataType::FixedSizeBinary(_) => Ok(Value::Bytes(
            downcast_array::<FixedSizeBinaryArray>(array)?
                .value(row_idx)
                .to_vec(),
        )),
        ArrowDataType::Date32 => Ok(Value::Date(
            downcast_array::<Date32Array>(array)?.value(row_idx),
        )),
        ArrowDataType::Date64 => {
            let millis = downcast_array::<Date64Array>(array)?.value(row_idx);
            let days = millis.div_euclid(MILLIS_PER_DAY);
            Ok(Value::Date(i32::try_from(days)?))
        }
        ArrowDataType::Timestamp(TimeUnit::Second, _) => {
            let seconds = downcast_array::<TimestampSecondArray>(array)?.value(row_idx);
            Ok(Value::Timestamp(seconds.checked_mul(1000).ok_or_else(
                || anyhow!("Timestamp conversion overflow for seconds value {seconds}"),
            )?))
        }
        ArrowDataType::Timestamp(TimeUnit::Millisecond, _) => Ok(Value::Timestamp(
            downcast_array::<TimestampMillisecondArray>(array)?.value(row_idx),
        )),
        ArrowDataType::Timestamp(TimeUnit::Microsecond, _) => Ok(Value::Timestamp(
            downcast_array::<TimestampMicrosecondArray>(array)?
                .value(row_idx)
                .div_euclid(1000),
        )),
        ArrowDataType::Timestamp(TimeUnit::Nanosecond, _) => Ok(Value::Timestamp(
            downcast_array::<TimestampNanosecondArray>(array)?
                .value(row_idx)
                .div_euclid(1_000_000),
        )),
        ArrowDataType::Decimal128(precision, scale) => {
            if *precision > 28 {
                return Err(anyhow!(
                    "Parquet Decimal128 with precision {} exceeds pg-tikv's maximum precision of 28",
                    precision
                ));
            }
            let raw = downcast_array::<Decimal128Array>(array)?.value(row_idx);
            Ok(Value::Numeric(decimal_from_parts(raw, *scale)?))
        }
        ArrowDataType::Dictionary(_, value_type) => {
            let casted = cast(array, value_type)?;
            arrow_array_to_value(casted.as_ref(), row_idx)
        }
        ArrowDataType::List(_)
        | ArrowDataType::LargeList(_)
        | ArrowDataType::Struct(_)
        | ArrowDataType::Map(_, _) => {
            let json = arrow_value_to_json(array, row_idx)?;
            Ok(Value::Jsonb(json.to_string()))
        }
        ArrowDataType::Null => Ok(Value::Null),
        unsupported => Err(anyhow!("Unsupported Arrow type: {:?}", unsupported)),
    }
}

fn downcast_array<T: 'static>(array: &dyn Array) -> Result<&T> {
    array.as_any().downcast_ref::<T>().ok_or_else(|| {
        anyhow!(
            "Failed to downcast Arrow array for type {:?}",
            array.data_type()
        )
    })
}

fn decimal_from_parts(raw: i128, scale: i8) -> Result<Decimal> {
    if scale >= 0 {
        return Ok(Decimal::try_from_i128_with_scale(raw, scale as u32)?);
    }

    let shift = u32::from((-scale) as u8);
    let multiplier = 10_i128
        .checked_pow(shift)
        .ok_or_else(|| anyhow!("Decimal scale conversion overflow for scale {scale}"))?;
    let widened = raw
        .checked_mul(multiplier)
        .ok_or_else(|| anyhow!("Decimal value overflow for scale {scale}"))?;
    Ok(Decimal::try_from_i128_with_scale(widened, 0)?)
}

fn arrow_value_to_json(array: &dyn Array, row_idx: usize) -> Result<JsonValue> {
    if array.is_null(row_idx) {
        return Ok(JsonValue::Null);
    }

    match array.data_type() {
        ArrowDataType::Boolean => Ok(JsonValue::Bool(
            downcast_array::<BooleanArray>(array)?.value(row_idx),
        )),
        ArrowDataType::Int8 => Ok(JsonValue::Number(JsonNumber::from(
            downcast_array::<Int8Array>(array)?.value(row_idx),
        ))),
        ArrowDataType::Int16 => Ok(JsonValue::Number(JsonNumber::from(
            downcast_array::<Int16Array>(array)?.value(row_idx),
        ))),
        ArrowDataType::Int32 => Ok(JsonValue::Number(JsonNumber::from(
            downcast_array::<Int32Array>(array)?.value(row_idx),
        ))),
        ArrowDataType::Int64 => Ok(JsonValue::Number(JsonNumber::from(
            downcast_array::<Int64Array>(array)?.value(row_idx),
        ))),
        ArrowDataType::UInt8 => Ok(JsonValue::Number(JsonNumber::from(
            downcast_array::<UInt8Array>(array)?.value(row_idx),
        ))),
        ArrowDataType::UInt16 => Ok(JsonValue::Number(JsonNumber::from(
            downcast_array::<UInt16Array>(array)?.value(row_idx),
        ))),
        ArrowDataType::UInt32 => Ok(JsonValue::Number(JsonNumber::from(
            downcast_array::<UInt32Array>(array)?.value(row_idx),
        ))),
        ArrowDataType::UInt64 => Ok(JsonValue::Number(JsonNumber::from(
            downcast_array::<UInt64Array>(array)?.value(row_idx),
        ))),
        ArrowDataType::Float16 => {
            let v = downcast_array::<Float16Array>(array)?.value(row_idx);
            float_to_json(f64::from(f32::from(v)))
        }
        ArrowDataType::Float32 => {
            float_to_json(downcast_array::<Float32Array>(array)?.value(row_idx) as f64)
        }
        ArrowDataType::Float64 => {
            float_to_json(downcast_array::<Float64Array>(array)?.value(row_idx))
        }
        ArrowDataType::Utf8 => Ok(JsonValue::String(
            downcast_array::<StringArray>(array)?
                .value(row_idx)
                .to_string(),
        )),
        ArrowDataType::LargeUtf8 => Ok(JsonValue::String(
            downcast_array::<LargeStringArray>(array)?
                .value(row_idx)
                .to_string(),
        )),
        ArrowDataType::Binary => Ok(JsonValue::Array(
            downcast_array::<BinaryArray>(array)?
                .value(row_idx)
                .iter()
                .map(|b| JsonValue::Number(JsonNumber::from(*b)))
                .collect(),
        )),
        ArrowDataType::LargeBinary => Ok(JsonValue::Array(
            downcast_array::<LargeBinaryArray>(array)?
                .value(row_idx)
                .iter()
                .map(|b| JsonValue::Number(JsonNumber::from(*b)))
                .collect(),
        )),
        ArrowDataType::FixedSizeBinary(_) => Ok(JsonValue::Array(
            downcast_array::<FixedSizeBinaryArray>(array)?
                .value(row_idx)
                .iter()
                .map(|b| JsonValue::Number(JsonNumber::from(*b)))
                .collect(),
        )),
        ArrowDataType::Date32 => Ok(JsonValue::Number(JsonNumber::from(
            downcast_array::<Date32Array>(array)?.value(row_idx),
        ))),
        ArrowDataType::Date64 => {
            let millis = downcast_array::<Date64Array>(array)?.value(row_idx);
            Ok(JsonValue::Number(JsonNumber::from(
                millis.div_euclid(MILLIS_PER_DAY),
            )))
        }
        ArrowDataType::Timestamp(TimeUnit::Second, _) => Ok(JsonValue::Number(JsonNumber::from(
            downcast_array::<TimestampSecondArray>(array)?
                .value(row_idx)
                .checked_mul(1000)
                .ok_or_else(|| anyhow!("Timestamp conversion overflow"))?,
        ))),
        ArrowDataType::Timestamp(TimeUnit::Millisecond, _) => Ok(JsonValue::Number(
            JsonNumber::from(downcast_array::<TimestampMillisecondArray>(array)?.value(row_idx)),
        )),
        ArrowDataType::Timestamp(TimeUnit::Microsecond, _) => {
            Ok(JsonValue::Number(JsonNumber::from(
                downcast_array::<TimestampMicrosecondArray>(array)?
                    .value(row_idx)
                    .div_euclid(1000),
            )))
        }
        ArrowDataType::Timestamp(TimeUnit::Nanosecond, _) => {
            Ok(JsonValue::Number(JsonNumber::from(
                downcast_array::<TimestampNanosecondArray>(array)?
                    .value(row_idx)
                    .div_euclid(1_000_000),
            )))
        }
        ArrowDataType::Decimal128(precision, scale) => {
            if *precision > 28 {
                return Err(anyhow!(
                    "Parquet Decimal128 with precision {} exceeds pg-tikv's maximum precision of 28",
                    precision
                ));
            }
            let raw = downcast_array::<Decimal128Array>(array)?.value(row_idx);
            Ok(JsonValue::String(
                decimal_from_parts(raw, *scale)?.to_string(),
            ))
        }
        ArrowDataType::Dictionary(_, value_type) => {
            let casted = cast(array, value_type)?;
            arrow_value_to_json(casted.as_ref(), row_idx)
        }
        ArrowDataType::List(_) => {
            let list = downcast_array::<ListArray>(array)?;
            let values = list.value(row_idx);
            let mut out = Vec::with_capacity(values.len());
            for i in 0..values.len() {
                out.push(arrow_value_to_json(values.as_ref(), i)?);
            }
            Ok(JsonValue::Array(out))
        }
        ArrowDataType::LargeList(_) => {
            let list = downcast_array::<LargeListArray>(array)?;
            let values = list.value(row_idx);
            let mut out = Vec::with_capacity(values.len());
            for i in 0..values.len() {
                out.push(arrow_value_to_json(values.as_ref(), i)?);
            }
            Ok(JsonValue::Array(out))
        }
        ArrowDataType::Struct(_) => {
            let struct_array = downcast_array::<StructArray>(array)?;
            let mut obj = JsonMap::new();
            for (idx, field) in struct_array.fields().iter().enumerate() {
                obj.insert(
                    field.name().to_string(),
                    arrow_value_to_json(struct_array.column(idx).as_ref(), row_idx)?,
                );
            }
            Ok(JsonValue::Object(obj))
        }
        ArrowDataType::Map(_, _) => {
            let map_array = downcast_array::<MapArray>(array)?;
            let entries = map_array.value(row_idx);
            let entries_struct = downcast_array::<StructArray>(&entries)?;
            let keys = entries_struct.column(0);
            let values = entries_struct.column(1);

            let mut obj = JsonMap::new();
            for i in 0..entries_struct.len() {
                let key = json_key_to_string(arrow_value_to_json(keys.as_ref(), i)?);
                let value = arrow_value_to_json(values.as_ref(), i)?;
                obj.insert(key, value);
            }
            Ok(JsonValue::Object(obj))
        }
        ArrowDataType::Null => Ok(JsonValue::Null),
        unsupported => Err(anyhow!("Unsupported Arrow type: {:?}", unsupported)),
    }
}

fn json_key_to_string(value: JsonValue) -> String {
    match value {
        JsonValue::String(s) => s,
        JsonValue::Number(n) => n.to_string(),
        JsonValue::Bool(b) => b.to_string(),
        JsonValue::Null => "null".to_string(),
        other => other.to_string(),
    }
}

fn float_to_json(v: f64) -> Result<JsonValue> {
    let number = JsonNumber::from_f64(v)
        .ok_or_else(|| anyhow!("Cannot serialize non-finite float value {v} to JSON"))?;
    Ok(JsonValue::Number(number))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use arrow_array::builder::{
        Int32Builder, ListBuilder, MapBuilder, StringBuilder, StringDictionaryBuilder,
        StructBuilder,
    };
    use arrow_array::{ArrayRef, Date64Array, Int32DictionaryArray, NullArray, UInt64Array};
    use arrow_schema::{DataType as ArrowDataType, Field};

    #[test]
    fn maps_boolean_type() {
        assert_eq!(
            arrow_type_to_pg_type(&ArrowDataType::Boolean).expect("boolean mapping should succeed"),
            DataType::Boolean
        );
    }

    #[test]
    fn maps_int_types() {
        assert_eq!(
            arrow_type_to_pg_type(&ArrowDataType::Int8).expect("int8 mapping"),
            DataType::Int32
        );
        assert_eq!(
            arrow_type_to_pg_type(&ArrowDataType::Int16).expect("int16 mapping"),
            DataType::Int32
        );
        assert_eq!(
            arrow_type_to_pg_type(&ArrowDataType::Int32).expect("int32 mapping"),
            DataType::Int32
        );
        assert_eq!(
            arrow_type_to_pg_type(&ArrowDataType::Int64).expect("int64 mapping"),
            DataType::Int64
        );
    }

    #[test]
    fn maps_unsigned_types() {
        assert_eq!(
            arrow_type_to_pg_type(&ArrowDataType::UInt8).expect("uint8 mapping"),
            DataType::Int32
        );
        assert_eq!(
            arrow_type_to_pg_type(&ArrowDataType::UInt16).expect("uint16 mapping"),
            DataType::Int32
        );
        assert_eq!(
            arrow_type_to_pg_type(&ArrowDataType::UInt32).expect("uint32 mapping"),
            DataType::Int64
        );
        assert_eq!(
            arrow_type_to_pg_type(&ArrowDataType::UInt64).expect("uint64 mapping"),
            DataType::Numeric {
                precision: None,
                scale: None
            }
        );
    }

    #[test]
    fn maps_float_and_text_types() {
        assert_eq!(
            arrow_type_to_pg_type(&ArrowDataType::Float16).expect("float16 mapping"),
            DataType::Float64
        );
        assert_eq!(
            arrow_type_to_pg_type(&ArrowDataType::Float32).expect("float32 mapping"),
            DataType::Float64
        );
        assert_eq!(
            arrow_type_to_pg_type(&ArrowDataType::Float64).expect("float64 mapping"),
            DataType::Float64
        );
        assert_eq!(
            arrow_type_to_pg_type(&ArrowDataType::Utf8).expect("utf8 mapping"),
            DataType::Text
        );
        assert_eq!(
            arrow_type_to_pg_type(&ArrowDataType::LargeUtf8).expect("large utf8 mapping"),
            DataType::Text
        );
    }

    #[test]
    fn maps_binary_and_date_types() {
        assert_eq!(
            arrow_type_to_pg_type(&ArrowDataType::Binary).expect("binary mapping"),
            DataType::Bytes
        );
        assert_eq!(
            arrow_type_to_pg_type(&ArrowDataType::LargeBinary).expect("large binary mapping"),
            DataType::Bytes
        );
        assert_eq!(
            arrow_type_to_pg_type(&ArrowDataType::FixedSizeBinary(2))
                .expect("fixed binary mapping"),
            DataType::Bytes
        );
        assert_eq!(
            arrow_type_to_pg_type(&ArrowDataType::Date32).expect("date32 mapping"),
            DataType::Date
        );
        assert_eq!(
            arrow_type_to_pg_type(&ArrowDataType::Date64).expect("date64 mapping"),
            DataType::Date
        );
    }

    #[test]
    fn maps_timestamp_decimal_dictionary_and_nested_types() {
        assert_eq!(
            arrow_type_to_pg_type(&ArrowDataType::Timestamp(TimeUnit::Second, None))
                .expect("timestamp second mapping"),
            DataType::Timestamp
        );
        assert_eq!(
            arrow_type_to_pg_type(&ArrowDataType::Decimal128(28, 2)).expect("decimal mapping"),
            DataType::Numeric {
                precision: Some(28),
                scale: Some(2)
            }
        );
        assert_eq!(
            arrow_type_to_pg_type(&ArrowDataType::Dictionary(
                Box::new(ArrowDataType::Int32),
                Box::new(ArrowDataType::Utf8),
            ))
            .expect("dictionary mapping"),
            DataType::Text
        );
        assert_eq!(
            arrow_type_to_pg_type(&ArrowDataType::List(Arc::new(Field::new_list_field(
                ArrowDataType::Int32,
                true,
            ))))
            .expect("list mapping"),
            DataType::Jsonb
        );
        assert_eq!(
            arrow_type_to_pg_type(&ArrowDataType::LargeList(Arc::new(Field::new_list_field(
                ArrowDataType::Int32,
                true,
            ))))
            .expect("large list mapping"),
            DataType::Jsonb
        );
        assert_eq!(
            arrow_type_to_pg_type(&ArrowDataType::Struct(
                vec![Arc::new(Field::new("a", ArrowDataType::Int32, true,))].into()
            ))
            .expect("struct mapping"),
            DataType::Jsonb
        );
        assert_eq!(
            arrow_type_to_pg_type(&ArrowDataType::Map(
                Arc::new(Field::new(
                    "entries",
                    ArrowDataType::Struct(
                        vec![
                            Arc::new(Field::new("key", ArrowDataType::Utf8, false)),
                            Arc::new(Field::new("value", ArrowDataType::Int32, true)),
                        ]
                        .into(),
                    ),
                    false,
                )),
                false,
            ))
            .expect("map mapping"),
            DataType::Jsonb
        );
        assert_eq!(
            arrow_type_to_pg_type(&ArrowDataType::Null).expect("null mapping"),
            DataType::Text
        );
    }

    #[test]
    fn decimal_precision_boundary() {
        assert!(arrow_type_to_pg_type(&ArrowDataType::Decimal128(28, 4)).is_ok());
        let err = arrow_type_to_pg_type(&ArrowDataType::Decimal128(29, 4))
            .expect_err("precision 29 should fail");
        assert!(
            err.to_string()
                .contains("exceeds pg-tikv's maximum precision of 28"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn value_boolean_and_null() {
        let array = BooleanArray::from(vec![Some(true), None]);
        assert_eq!(
            arrow_array_to_value(&array, 0).expect("boolean conversion should succeed"),
            Value::Boolean(true)
        );
        assert_eq!(
            arrow_array_to_value(&array, 1).expect("null conversion should succeed"),
            Value::Null
        );
    }

    #[test]
    fn value_signed_integers() {
        let int8 = Int8Array::from(vec![Some(-5)]);
        let int16 = Int16Array::from(vec![Some(-123)]);
        let int32 = Int32Array::from(vec![Some(456)]);
        let int64 = Int64Array::from(vec![Some(789_i64)]);
        assert_eq!(
            arrow_array_to_value(&int8, 0).expect("int8 conversion"),
            Value::Int32(-5)
        );
        assert_eq!(
            arrow_array_to_value(&int16, 0).expect("int16 conversion"),
            Value::Int32(-123)
        );
        assert_eq!(
            arrow_array_to_value(&int32, 0).expect("int32 conversion"),
            Value::Int32(456)
        );
        assert_eq!(
            arrow_array_to_value(&int64, 0).expect("int64 conversion"),
            Value::Int64(789)
        );
    }

    #[test]
    fn value_unsigned_integers_and_ranges() {
        let uint8 = UInt8Array::from(vec![Some(250)]);
        let uint16 = UInt16Array::from(vec![Some(65_000)]);
        let uint32 = UInt32Array::from(vec![Some(u32::MAX)]);
        let uint64 = UInt64Array::from(vec![Some(u64::MAX)]);

        assert_eq!(
            arrow_array_to_value(&uint8, 0).expect("uint8 conversion"),
            Value::Int32(250)
        );
        assert_eq!(
            arrow_array_to_value(&uint16, 0).expect("uint16 conversion"),
            Value::Int32(65_000)
        );
        assert_eq!(
            arrow_array_to_value(&uint32, 0).expect("uint32 conversion"),
            Value::Int64(u32::MAX as i64)
        );
        let numeric = arrow_array_to_value(&uint64, 0).expect("uint64 conversion should succeed");
        match numeric {
            Value::Numeric(v) => {
                let expected = Decimal::from_str_exact(&u64::MAX.to_string())
                    .expect("u64 max decimal should parse");
                assert_eq!(v, expected);
            }
            other => panic!("expected numeric, got {other:?}"),
        }
    }

    #[test]
    fn value_floats() {
        let f32_array = Float32Array::from(vec![Some(3.5)]);
        let f64_array = Float64Array::from(vec![Some(9.25)]);

        let f16_source = Float32Array::from(vec![Some(1.5)]);
        let f16 = cast(&f16_source, &ArrowDataType::Float16).expect("cast float32->float16");

        assert_eq!(
            arrow_array_to_value(f16.as_ref(), 0).expect("float16 conversion"),
            Value::Float64(1.5)
        );
        assert_eq!(
            arrow_array_to_value(&f32_array, 0).expect("float32 conversion"),
            Value::Float64(3.5)
        );
        assert_eq!(
            arrow_array_to_value(&f64_array, 0).expect("float64 conversion"),
            Value::Float64(9.25)
        );
    }

    #[test]
    fn value_utf8_and_binary() {
        let utf8 = StringArray::from(vec![Some("hello")]);
        let large_utf8_source = StringArray::from(vec![Some("world")]);
        let large_utf8 = cast(&large_utf8_source, &ArrowDataType::LargeUtf8)
            .expect("cast utf8->largeutf8 should succeed");

        let binary = BinaryArray::from(vec![Some(b"ab".as_slice())]);
        let large_binary_source = BinaryArray::from(vec![Some(b"cd".as_slice())]);
        let large_binary = cast(&large_binary_source, &ArrowDataType::LargeBinary)
            .expect("cast binary->largebinary should succeed");
        let fixed_binary = FixedSizeBinaryArray::try_from_iter(vec![vec![7_u8, 8_u8]].into_iter())
            .expect("fixed binary build should succeed");

        assert_eq!(
            arrow_array_to_value(&utf8, 0).expect("utf8 conversion"),
            Value::Text("hello".to_string())
        );
        assert_eq!(
            arrow_array_to_value(large_utf8.as_ref(), 0).expect("large utf8 conversion"),
            Value::Text("world".to_string())
        );
        assert_eq!(
            arrow_array_to_value(&binary, 0).expect("binary conversion"),
            Value::Bytes(vec![97, 98])
        );
        assert_eq!(
            arrow_array_to_value(large_binary.as_ref(), 0).expect("large binary conversion"),
            Value::Bytes(vec![99, 100])
        );
        assert_eq!(
            arrow_array_to_value(&fixed_binary, 0).expect("fixed binary conversion"),
            Value::Bytes(vec![7, 8])
        );
    }

    #[test]
    fn value_dates() {
        let date32 = Date32Array::from(vec![Some(1234)]);
        let date64 = Date64Array::from(vec![Some(2 * MILLIS_PER_DAY + 42)]);

        assert_eq!(
            arrow_array_to_value(&date32, 0).expect("date32 conversion"),
            Value::Date(1234)
        );
        assert_eq!(
            arrow_array_to_value(&date64, 0).expect("date64 conversion"),
            Value::Date(2)
        );
    }

    #[test]
    fn value_timestamp_seconds_to_millis() {
        let ts = TimestampSecondArray::from(vec![Some(7)]);
        assert_eq!(
            arrow_array_to_value(&ts, 0).expect("timestamp second conversion"),
            Value::Timestamp(7000)
        );
    }

    #[test]
    fn value_timestamp_millis_kept() {
        let ts = TimestampMillisecondArray::from(vec![Some(1234)]);
        assert_eq!(
            arrow_array_to_value(&ts, 0).expect("timestamp millisecond conversion"),
            Value::Timestamp(1234)
        );
    }

    #[test]
    fn value_timestamp_micros_to_millis() {
        let ts = TimestampMicrosecondArray::from(vec![Some(9_876)]);
        assert_eq!(
            arrow_array_to_value(&ts, 0).expect("timestamp microsecond conversion"),
            Value::Timestamp(9)
        );
    }

    #[test]
    fn value_timestamp_nanos_to_millis() {
        let ts = TimestampNanosecondArray::from(vec![Some(12_345_678)]);
        assert_eq!(
            arrow_array_to_value(&ts, 0).expect("timestamp nanosecond conversion"),
            Value::Timestamp(12)
        );
    }

    #[test]
    fn value_decimal128_boundary() {
        let arr_ok = Decimal128Array::from_iter_values([12345])
            .with_precision_and_scale(28, 2)
            .expect("decimal metadata should be valid");
        let value = arrow_array_to_value(&arr_ok, 0).expect("decimal conversion should succeed");
        match value {
            Value::Numeric(v) => {
                let expected = Decimal::from_str_exact("123.45")
                    .expect("expected decimal literal should parse");
                assert_eq!(v, expected);
            }
            other => panic!("expected numeric value, got {other:?}"),
        }

        let arr_err = Decimal128Array::from_iter_values([1])
            .with_precision_and_scale(29, 0)
            .expect("decimal metadata should be valid");
        let err = arrow_array_to_value(&arr_err, 0).expect_err("precision 29 should fail");
        assert!(
            err.to_string()
                .contains("exceeds pg-tikv's maximum precision of 28"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn value_dictionary_unwrap() {
        let dict: Int32DictionaryArray =
            Int32DictionaryArray::from_iter([Some("red"), Some("blue")]);
        assert_eq!(
            arrow_array_to_value(&dict, 1).expect("dictionary conversion should succeed"),
            Value::Text("blue".to_string())
        );
    }

    #[test]
    fn value_list_to_jsonb() {
        let mut builder = ListBuilder::new(Int32Builder::new());
        builder.append_value([Some(1), Some(2), None]);
        builder.append_value([Some(3)]);
        let list = builder.finish();

        assert_eq!(
            arrow_array_to_value(&list, 0).expect("list conversion should succeed"),
            Value::Jsonb("[1,2,null]".to_string())
        );
        assert_eq!(
            arrow_array_to_value(&list, 1).expect("list conversion should succeed"),
            Value::Jsonb("[3]".to_string())
        );
    }

    #[test]
    fn value_large_list_to_jsonb() {
        let mut builder = ListBuilder::new(Int32Builder::new());
        builder.append_value([Some(4), Some(5)]);
        let list = builder.finish();
        let large_list_type =
            ArrowDataType::LargeList(Arc::new(Field::new_list_field(ArrowDataType::Int32, true)));
        let large = cast(&list, &large_list_type).expect("cast list->large_list should succeed");

        assert_eq!(
            arrow_array_to_value(large.as_ref(), 0).expect("large list conversion should succeed"),
            Value::Jsonb("[4,5]".to_string())
        );
    }

    #[test]
    fn value_struct_to_jsonb() {
        let fields = vec![
            Field::new("id", ArrowDataType::Int32, true),
            Field::new("name", ArrowDataType::Utf8, true),
        ];
        let mut builder = StructBuilder::new(
            fields,
            vec![
                Box::new(Int32Builder::new()),
                Box::new(StringBuilder::new()),
            ],
        );
        builder
            .field_builder::<Int32Builder>(0)
            .expect("field 0 should be int32")
            .append_value(7);
        builder
            .field_builder::<StringBuilder>(1)
            .expect("field 1 should be string")
            .append_value("alice");
        builder.append(true);
        let struct_array = builder.finish();

        assert_eq!(
            arrow_array_to_value(&struct_array, 0).expect("struct conversion should succeed"),
            Value::Jsonb("{\"id\":7,\"name\":\"alice\"}".to_string())
        );
    }

    #[test]
    fn value_map_to_jsonb() {
        let mut builder = MapBuilder::new(None, StringBuilder::new(), Int32Builder::new());
        builder.keys().append_value("k1");
        builder.values().append_value(10);
        builder.keys().append_value("k2");
        builder.values().append_value(20);
        builder.append(true).expect("map row append should succeed");
        let map = builder.finish();

        assert_eq!(
            arrow_array_to_value(&map, 0).expect("map conversion should succeed"),
            Value::Jsonb("{\"k1\":10,\"k2\":20}".to_string())
        );
    }

    #[test]
    fn null_array_returns_null_value() {
        let array = NullArray::new(2);
        assert_eq!(
            arrow_array_to_value(&array, 1).expect("null array conversion should succeed"),
            Value::Null
        );
    }

    #[test]
    fn null_short_circuit_for_non_null_type() {
        let array = Int32Array::from(vec![None, Some(5)]);
        assert_eq!(
            arrow_array_to_value(&array, 0).expect("null entry conversion should succeed"),
            Value::Null
        );
        assert_eq!(
            arrow_array_to_value(&array, 1).expect("non-null entry conversion should succeed"),
            Value::Int32(5)
        );
    }

    #[test]
    fn dictionary_null_short_circuit() {
        let mut builder = StringDictionaryBuilder::<arrow_array::types::Int8Type>::new();
        builder.append("a").expect("append first dictionary value");
        builder.append_null();
        let dict = builder.finish();

        assert_eq!(
            arrow_array_to_value(&dict, 1).expect("dictionary null conversion should succeed"),
            Value::Null
        );
    }

    #[test]
    fn unsupported_type_returns_error() {
        let err = arrow_type_to_pg_type(&ArrowDataType::Duration(arrow_schema::TimeUnit::Second))
            .expect_err("duration type should be unsupported");
        assert!(err.to_string().contains("Unsupported Arrow type"));
    }

    #[test]
    fn row_index_bounds_check() {
        let array = UInt64Array::from(vec![Some(1)]);
        let err = arrow_array_to_value(&array, 1).expect_err("out-of-bounds row should fail");
        assert!(err.to_string().contains("out of bounds"));
    }

    #[test]
    fn dictionary_cast_path_for_numeric() {
        let dict = Int32DictionaryArray::from_iter([Some("42"), Some("7")]);
        let casted: ArrayRef =
            cast(&dict, &ArrowDataType::Utf8).expect("dictionary cast should work");
        assert_eq!(
            arrow_array_to_value(casted.as_ref(), 0).expect("casted dictionary value conversion"),
            Value::Text("42".to_string())
        );
    }
}
