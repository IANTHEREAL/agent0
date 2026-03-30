//! Shared datum-size helpers for width estimation.
//!
//! These functions estimate datum widths for numeric and array types,
//! matching PostgreSQL's on-disk layout for those types.  They are used by:
//! - `ANALYZE` (WIDTH_THRESHOLD gating)
//! - `pg_column_size()` SQL function (numeric and array sizing)
//!
//! Note: jsonb, tsvector, and tsquery use text-length approximation
//! (`s.len() + 4`) which does NOT match PG's binary storage format.
//! Accurate binary format estimation for these types is tracked separately.

use crate::model::Value;

// ── Numeric sizing ─────────────────────────────────────────────────────

/// Estimate PG numeric datum width based on actual digit count.
///
/// PG stores numeric in base-10000 (NBASE) groups, stripping trailing
/// zero groups from canonical storage.  Each element uses a 4-byte
/// varlena header + 2-byte numeric header + 2 bytes per NBASE group
/// = `6 + 2 * ngroups`.
///
/// Verified against PG 17.9 array element sizes (per-element after INTALIGN):
///   1::numeric         → 8/elem   (1 group,  datum=8)
///   1.23::numeric      → 12/elem  (2 groups, datum=10, INTALIGN→12)
///   10000::numeric     → 8/elem   (1 group,  trailing zeros stripped)
///   1000000000000      → 8/elem   (1 group,  trailing zeros stripped)
///   1.0::numeric       → 8/elem   (1 group,  trailing .0 stripped)
pub(crate) fn numeric_datum_width(d: &rust_decimal::Decimal) -> usize {
    if d.is_zero() {
        // PG zero numeric: 4 (varlena) + 2 (header) + 0 groups = 6.
        return 6;
    }
    let mantissa = d.mantissa().unsigned_abs();
    let scale = d.scale() as usize;

    // Count significant base-10 digits in the mantissa.
    let sig_digits = base10_digit_count(mantissa);

    // Count trailing zeros — PG strips trailing zero NBASE groups from
    // canonical storage, so trailing decimal zeros don't contribute groups.
    let trailing_zeros = trailing_zero_count(mantissa);

    // Determine positions of most/least significant digits relative to
    // the decimal point (position 0 = units digit).
    let ms_pos = if sig_digits > scale {
        (sig_digits - scale) as i64 - 1
    } else {
        -((scale - sig_digits) as i64) - 1
    };
    // Position of least significant NON-ZERO digit (after stripping).
    let ls_pos = -(scale as i64) + trailing_zeros as i64;

    // Map digit positions to PG NBASE groups.
    // Group at weight w covers positions [4w, 4w+3].
    let ms_group = ms_pos.div_euclid(4);
    let ls_group = ls_pos.div_euclid(4);
    let ngroups = (ms_group - ls_group + 1) as usize;

    // 4 (varlena header) + 2 (numeric header) + 2 bytes per NBASE group.
    6 + 2 * ngroups
}

/// Count base-10 digits in a u128 value.
pub(crate) fn base10_digit_count(n: u128) -> usize {
    if n == 0 {
        return 1;
    }
    let mut digits = 1;
    let mut threshold: u128 = 10;
    while threshold <= n && digits < 39 {
        digits += 1;
        // Guard against overflow: u128 max is ~3.4e38 (39 digits)
        threshold = threshold.saturating_mul(10);
    }
    digits
}

/// Count trailing decimal zeros in a u128 value.
pub(crate) fn trailing_zero_count(mut n: u128) -> usize {
    if n == 0 {
        return 0;
    }
    let mut count = 0;
    while n.is_multiple_of(10) {
        count += 1;
        n /= 10;
    }
    count
}

// ── Array sizing ───────────────────────────────────────────────────────

/// Compute datum width for an array value, matching PG's physical layout.
///
/// PG stores multi-dimensional arrays as a single flat structure with one
/// header (not nested headers per dimension).  Uses per-type element
/// alignment (pg_type.typalign) instead of blanket INTALIGN.
///
/// Verified against PG 17.9:
///   array_fill(true, array[251])  -> 275  (1/elem, typalign='c')
///   array_fill(true, array[300])  -> 324  (1/elem, typalign='c')
///   array[1]::int4[]              -> 28   (4/elem, typalign='i')
pub(crate) fn array_datum_width(elements: &[Value]) -> usize {
    if elements.is_empty() {
        return 16; // PG zero-dimension empty array
    }

    // PG collapses arrays with any zero-length dimension to a zero-dimensional
    // empty array (pg_column_size = 16). E.g. ARRAY[ARRAY[]::int4[]] = 16.
    let leaf_count = array_leaf_count(elements);
    if leaf_count == 0 {
        return 16;
    }

    // PG array header: 16 + 8 * ndim
    let ndim = array_ndim(elements);
    let header = 16 + 8 * ndim;
    let null_count = array_null_count(elements);
    let null_bitmap = if null_count > 0 {
        leaf_count.div_ceil(8)
    } else {
        0
    };
    let elem_size = array_leaf_size(elements);

    // MAXALIGN the header+bitmap before data payload.
    let header_plus_bitmap = header + null_bitmap;
    let aligned = (header_plus_bitmap + 7) & !7;
    aligned + elem_size
}

/// Nesting depth: 1 for flat arrays, 2+ for multi-dimensional.
pub(crate) fn array_ndim(elements: &[Value]) -> usize {
    match elements.first() {
        Some(Value::Array(inner)) => 1 + array_ndim(inner),
        _ => 1,
    }
}

/// Count leaf (non-array) elements across all nesting levels.
pub(crate) fn array_leaf_count(elements: &[Value]) -> usize {
    elements
        .iter()
        .map(|v| match v {
            Value::Array(inner) => array_leaf_count(inner),
            _ => 1,
        })
        .sum()
}

/// Count null leaf elements across all nesting levels.
pub(crate) fn array_null_count(elements: &[Value]) -> usize {
    elements
        .iter()
        .map(|v| match v {
            Value::Null => 1,
            Value::Array(inner) => array_null_count(inner),
            _ => 0,
        })
        .sum()
}

/// Sum of aligned leaf element sizes (no array headers).
///
/// Uses `datum_width` for individual leaf elements and applies
/// per-type alignment via `pg_type_align`.
pub(crate) fn array_leaf_size(elements: &[Value]) -> usize {
    elements
        .iter()
        .filter(|v| !matches!(v, Value::Null))
        .map(|v| match v {
            Value::Array(inner) => array_leaf_size(inner),
            other => {
                let w = leaf_datum_width(other);
                pg_type_align(w, other)
            }
        })
        .sum()
}

/// Datum width for a non-array leaf value (used inside array_leaf_size).
///
/// This is the same logic as the top-level datum_width but must not be
/// called on `Value::Array` (arrays are recursed into by array_leaf_size).
fn leaf_datum_width(value: &Value) -> usize {
    match value {
        Value::Null => 0,
        Value::Boolean(_) => 1,
        Value::Int32(_) => 4,
        Value::Int64(_) | Value::Float64(_) => 8,
        Value::Numeric(d) => numeric_datum_width(d),
        Value::Date(_) => 4,
        Value::Time(_) | Value::Timestamp(_) => 8,
        Value::Interval { .. } => 16,
        Value::Uuid(_) => 16,
        Value::Text(s) => s.len() + 4,
        Value::Json(s) | Value::Jsonb(s) => s.len() + 4,
        Value::Bytes(b) => b.len() + 4,
        Value::Tsvector(s) | Value::Tsquery(s) => s.len() + 4,
        Value::Vector(v) => v.len() * 4 + 4,
        Value::Array(_) => unreachable!("leaf_datum_width called on Array"),
    }
}

// ── Type alignment ─────────────────────────────────────────────────────

/// Apply PG per-type alignment to a datum width.
/// Matches pg_type.typalign from the PostgreSQL system catalog.
pub(crate) fn pg_type_align(width: usize, value: &Value) -> usize {
    let align = match value {
        // typalign = 'c' (char = 1 byte): no padding
        Value::Boolean(_) | Value::Uuid(_) => 1,
        // typalign = 'i' (int = 4 bytes)
        Value::Int32(_) | Value::Date(_) => 4,
        // typalign = 'd' (double = 8 bytes)
        Value::Int64(_)
        | Value::Float64(_)
        | Value::Time(_)
        | Value::Timestamp(_)
        | Value::Interval { .. } => 8,
        // Varlena types: typalign = 'i' (4 bytes)
        Value::Numeric(_)
        | Value::Text(_)
        | Value::Json(_)
        | Value::Jsonb(_)
        | Value::Bytes(_)
        | Value::Tsvector(_)
        | Value::Tsquery(_)
        | Value::Vector(_)
        | Value::Array(_) => 4,
        Value::Null => 1,
    };
    (width + align - 1) & !(align - 1)
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::Value;

    /// Convenience: compute datum width for a single value using the same
    /// dispatch as `pg_datum_width` / `pg_column_size`.
    fn datum_width(value: &Value) -> usize {
        match value {
            Value::Null => 0,
            Value::Boolean(_) => 1,
            Value::Int32(_) => 4,
            Value::Int64(_) | Value::Float64(_) => 8,
            Value::Numeric(d) => numeric_datum_width(d),
            Value::Date(_) => 4,
            Value::Time(_) | Value::Timestamp(_) => 8,
            Value::Interval { .. } => 16,
            Value::Uuid(_) => 16,
            Value::Text(s) => s.len() + 4,
            Value::Json(s) | Value::Jsonb(s) => s.len() + 4,
            Value::Bytes(b) => b.len() + 4,
            Value::Tsvector(s) | Value::Tsquery(s) => s.len() + 4,
            Value::Vector(v) => v.len() * 4 + 4,
            Value::Array(a) => array_datum_width(a),
        }
    }

    // ── Scalar datum width tests ──

    #[test]
    fn datum_width_scalar_types() {
        assert_eq!(datum_width(&Value::Null), 0);
        assert_eq!(datum_width(&Value::Boolean(true)), 1);
        assert_eq!(datum_width(&Value::Int32(42)), 4);
        assert_eq!(datum_width(&Value::Int64(42)), 8);
        assert_eq!(datum_width(&Value::Float64(1.5)), 8);
        assert_eq!(datum_width(&Value::Date(0)), 4);
        assert_eq!(datum_width(&Value::Time(0)), 8);
        assert_eq!(datum_width(&Value::Timestamp(0)), 8);
        assert_eq!(datum_width(&Value::Uuid([0u8; 16])), 16);
        assert_eq!(datum_width(&Value::Text("hello".into())), 9); // 5 + 4

        // Json (text format) -- PG stores as text: len + 4 varlena.
        assert_eq!(
            datum_width(&Value::Json(r#"{"a":1}"#.to_string())),
            11 // 7 + 4
        );

        // Jsonb -- text-length approximation (NOT PG-accurate).
        // PG stores jsonb in binary format: pg_column_size('{"a":1}'::jsonb) = 28.
        // Accurate binary format estimation is a follow-up task.
        assert_eq!(
            datum_width(&Value::Jsonb(r#"{"a":1}"#.to_string())),
            11 // text approximation: 7 + 4 (PG binary = 28)
        );
    }

    // ── Numeric sizing tests ──

    /// Issue #2054: numeric datum_width must be value-dependent, not always 20.
    /// Verified against PG 17.9 array_fill measurements.
    #[test]
    fn datum_width_numeric_value_dependent() {
        use rust_decimal::Decimal;
        use std::str::FromStr;

        // 1::numeric -> per-element in PG array = 8, so datum_width should be
        // small enough that INTALIGN(datum_width) = 8, i.e. datum_width in [5,8].
        let d1 = Decimal::from(1);
        let w1 = datum_width(&Value::Numeric(d1));
        assert!(w1 <= 8, "1::numeric datum_width={w1}, expected <= 8");
        assert!((w1 + 3) & !3 == 8, "INTALIGN({w1}) should be 8");

        // 1.23::numeric -> INTALIGN = 12.
        let d2 = Decimal::from_str("1.23").unwrap();
        let w2 = datum_width(&Value::Numeric(d2));
        assert!(
            (w2 + 3) & !3 == 12,
            "INTALIGN({w2}) should be 12, got {}",
            (w2 + 3) & !3
        );

        // 28-digit number -> INTALIGN = 20.
        let d3 = Decimal::from_str("9999999999999999999999999999").unwrap();
        let w3 = datum_width(&Value::Numeric(d3));
        assert!(
            (w3 + 3) & !3 == 20,
            "INTALIGN({w3}) should be 20, got {}",
            (w3 + 3) & !3
        );

        // Zero should be small.
        let d0 = Decimal::from(0);
        assert!(datum_width(&Value::Numeric(d0)) <= 8);
    }

    /// QG P1: PG strips trailing zero NBASE groups from canonical numeric storage.
    #[test]
    fn datum_width_numeric_trailing_zero_groups_stripped() {
        use rust_decimal::Decimal;
        use std::str::FromStr;

        let d = Decimal::from_str("1.0").unwrap();
        let w = datum_width(&Value::Numeric(d));
        assert_eq!(w, 8, "1.0::numeric should be 8 (1 group), got {w}");

        let d = Decimal::from(10000);
        let w = datum_width(&Value::Numeric(d));
        assert_eq!(w, 8, "10000::numeric should be 8 (1 group), got {w}");

        let d = Decimal::from(1000000000000i64);
        let w = datum_width(&Value::Numeric(d));
        assert_eq!(
            w, 8,
            "1000000000000::numeric should be 8 (1 group), got {w}"
        );

        let d = Decimal::from_str("1234.0000").unwrap();
        let w = datum_width(&Value::Numeric(d));
        assert_eq!(w, 8, "1234.0000::numeric should be 8 (1 group), got {w}");
    }

    // ── Array sizing tests ──

    /// Issue #2054: array_fill(1::numeric, array[51]) must be below 1024.
    #[test]
    fn datum_width_numeric_array_below_threshold() {
        use rust_decimal::Decimal;

        let arr: Vec<Value> = (0..51).map(|_| Value::Numeric(Decimal::from(1))).collect();
        let width = datum_width(&Value::Array(arr));
        assert!(
            width <= 1024,
            "numeric[51] with small values: width={width}, threshold=1024"
        );
        assert!(width <= 500, "numeric[51] width={width}, expected ~432");
    }

    /// Issue #2055: bool arrays must use typalign='c' (1-byte), not INTALIGN.
    /// PG 17: array_fill(true, array[251]) = 275, array_fill(true, array[300]) = 324.
    #[test]
    fn datum_width_bool_array_char_aligned() {
        let arr251: Vec<Value> = (0..251).map(|_| Value::Boolean(true)).collect();
        let w251 = datum_width(&Value::Array(arr251));
        assert!(w251 <= 1024, "bool[251] width={w251}, must be <= 1024");
        assert_eq!(w251, 275, "bool[251] should match PG's 275");

        let arr300: Vec<Value> = (0..300).map(|_| Value::Boolean(true)).collect();
        let w300 = datum_width(&Value::Array(arr300));
        assert_eq!(w300, 324, "bool[300] should match PG's 324");
    }

    /// Issue #2055: int4 arrays use typalign='i' (4-byte).
    /// PG 17: array[1]::int4[] = 28 = 24 + 4.
    #[test]
    fn datum_width_int4_array() {
        let arr = vec![Value::Int32(1)];
        assert_eq!(datum_width(&Value::Array(arr)), 28);
    }

    /// Issue #2052: multi-dimensional arrays must NOT add nested headers.
    /// PG treats ARRAY[[1,2],[3,4]] as a flat 2-D array with one header.
    #[test]
    fn datum_width_multidim_array_no_nested_headers() {
        let arr = Value::Array(vec![
            Value::Array(vec![Value::Int32(1), Value::Int32(2)]),
            Value::Array(vec![Value::Int32(3), Value::Int32(4)]),
        ]);
        let w = datum_width(&arr);
        assert_eq!(w, 48, "2-D int4[2][2] should be 48, got {w}");
    }

    /// Issue #2050: pg_column_size() and ANALYZE datum_width use same logic.
    #[test]
    fn datum_width_array_consistent_with_pg_column_size() {
        let arr = vec![Value::Int32(1), Value::Int32(2), Value::Int32(3)];
        let analyze_width = datum_width(&Value::Array(arr));
        assert_eq!(analyze_width, 36);
    }

    /// QG V2 P1: arrays with zero-length dimensions collapse to empty (16 bytes).
    /// PG 17: pg_column_size(ARRAY[ARRAY[]::int4[]]) = 16.
    #[test]
    fn datum_width_zero_dimension_array_is_empty() {
        let arr = Value::Array(vec![Value::Array(vec![])]);
        assert_eq!(datum_width(&arr), 16, "zero-dim array should be 16");

        let arr2 = Value::Array(vec![Value::Array(vec![]), Value::Array(vec![])]);
        assert_eq!(datum_width(&arr2), 16, "all-empty subarrays should be 16");

        let arr3 = Value::Array(vec![]);
        assert_eq!(datum_width(&arr3), 16, "empty array should be 16");
    }

    /// Numeric array with trailing-zero values below threshold.
    #[test]
    fn datum_width_numeric_trailing_zeros_array() {
        use rust_decimal::Decimal;

        let arr: Vec<Value> = (0..100)
            .map(|_| Value::Numeric(Decimal::from(10000)))
            .collect();
        let width = datum_width(&Value::Array(arr));
        assert!(
            width <= 1024,
            "numeric[100] with 10000: width={width}, threshold=1024"
        );
    }
}
