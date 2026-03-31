//! Shared datum-size helpers for width estimation.
//!
//! These functions estimate datum widths for numeric, array, jsonb,
//! tsvector, and tsquery types, matching PostgreSQL's on-disk layout.
//! They are used by:
//! - `ANALYZE` (WIDTH_THRESHOLD gating)
//! - `pg_column_size()` SQL function

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

// ── JSONB binary format estimation ─────────────────────────────────────

/// Estimate PG binary format size for a jsonb datum.
///
/// PG stores jsonb as a binary tree of containers with 4-byte JEntry
/// headers per key/value. Falls back to `s.len() + 4` on parse failure.
///
/// Verified against PG 17:
///   null      → 12    true      → 12    42        → 20
///   "hello"   → 17    {}        → 8     {"a":1}   → 28
///   [1,2,3]   → 44    {"a":{"b":1}} → 44
pub(crate) fn jsonb_datum_width(s: &str) -> usize {
    match serde_json::from_str::<serde_json::Value>(s) {
        Ok(val) => {
            let mut offset = 4usize; // varlena header
            jsonb_emit_top(&val, &mut offset);
            offset
        }
        Err(_) => s.len() + 4, // fallback
    }
}

/// Emit a top-level JSON value.  Scalars are wrapped in a scalar-array
/// container; arrays and objects emit their own container directly.
fn jsonb_emit_top(val: &serde_json::Value, offset: &mut usize) {
    match val {
        serde_json::Value::Null | serde_json::Value::Bool(_) => {
            // Scalar wrapper: container header + 1 JEntry + 0 data bytes
            *offset += 4 + 4;
        }
        serde_json::Value::Number(n) => {
            *offset += 4 + 4; // container header + JEntry
            *offset = (*offset + 3) & !3; // INTALIGN before numeric
            *offset += jsonb_numeric_size(n);
        }
        serde_json::Value::String(s) => {
            *offset += 4 + 4; // container header + JEntry
            *offset += s.len();
        }
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            jsonb_emit_container(val, offset);
        }
    }
}

/// Emit an array or object container (not scalar-wrapped).
fn jsonb_emit_container(val: &serde_json::Value, offset: &mut usize) {
    match val {
        serde_json::Value::Array(arr) => {
            *offset += 4 + 4 * arr.len(); // container header + JEntries
            for elem in arr {
                jsonb_emit_element(elem, offset);
            }
        }
        serde_json::Value::Object(obj) => {
            let npairs = obj.len();
            *offset += 4 + 8 * npairs; // container header + key + value JEntries

            // PG sorts keys by length then lexicographic (lengthCompareJsonbStringValue)
            let mut pairs: Vec<_> = obj.iter().collect();
            pairs.sort_by(|(a, _), (b, _)| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));

            for (key, _) in &pairs {
                *offset += key.len(); // key data (strings, no alignment)
            }
            for (_, val) in &pairs {
                jsonb_emit_element(val, offset); // value data
            }
        }
        _ => {}
    }
}

/// Emit a value inside an array or object (no scalar wrapping).
fn jsonb_emit_element(val: &serde_json::Value, offset: &mut usize) {
    match val {
        serde_json::Value::Null | serde_json::Value::Bool(_) => {
            // 0 data bytes — type encoded in JEntry flags
        }
        serde_json::Value::Number(n) => {
            *offset = (*offset + 3) & !3; // INTALIGN
            *offset += jsonb_numeric_size(n);
        }
        serde_json::Value::String(s) => {
            *offset += s.len(); // no alignment for strings
        }
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            *offset = (*offset + 3) & !3; // INTALIGN for nested containers
            jsonb_emit_container(val, offset);
        }
    }
}

/// Size of a PG numeric datum for a JSON number (including varlena header).
fn jsonb_numeric_size(n: &serde_json::Number) -> usize {
    use rust_decimal::Decimal;
    use std::str::FromStr;

    let s = n.to_string();
    if let Ok(d) = Decimal::from_str(&s) {
        numeric_datum_width(&d)
    } else {
        8 // conservative minimum
    }
}

// ── Tsvector binary format estimation ─────────────────────────────────

/// Estimate PG binary format size for a tsvector datum.
///
/// PG stores tsvector as: [4B varlena][4B nlexemes][4B × n WordEntries]
/// followed by per-lexeme data: [bytes][SHORTALIGN pad][2B npos][2B × npos].
///
/// Verified against PG 17:
///   (empty)             → 8     'hello'             → 17
///   'hello':1           → 22    'hello':1 'world':2 → 36
///   'ab':1,2,3          → 22
pub(crate) fn tsvector_datum_width(s: &str) -> usize {
    if s.is_empty() {
        return 8; // 4 varlena + 4 nlexemes
    }

    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    // Phase 1: Parse all lexemes into (decoded_bytes, npos) pairs.
    // We need the decoded bytes for sorting — PG stores lexemes sorted by
    // (length, lexicographic) per compareWordEntryPos in tsvector.h, and the
    // cumulative data_offset (which drives SHORTALIGN padding) depends on
    // this sorted order.
    let mut lexemes: Vec<(Vec<u8>, usize)> = Vec::new();

    while i < len {
        // Skip whitespace
        while i < len && bytes[i] == b' ' {
            i += 1;
        }
        if i >= len {
            break;
        }

        // Expect opening single quote
        if bytes[i] != b'\'' {
            return s.len() + 4; // malformed, fallback
        }
        i += 1;

        // Read lexeme bytes until unescaped closing quote
        let mut lexeme_bytes: Vec<u8> = Vec::new();
        while i < len {
            if bytes[i] == b'\'' {
                i += 1;
                if i < len && bytes[i] == b'\'' {
                    i += 1; // escaped ''
                    lexeme_bytes.push(b'\'');
                } else {
                    break; // closing quote
                }
            } else if bytes[i] == b'\\' {
                i += 1;
                if i < len {
                    lexeme_bytes.push(bytes[i]);
                    i += 1;
                }
            } else {
                lexeme_bytes.push(bytes[i]);
                i += 1;
            }
        }

        // Count positions after ':'
        let mut npos = 0usize;
        if i < len && bytes[i] == b':' {
            i += 1;
            npos = 1;
            while i < len {
                if bytes[i] == b',' {
                    npos += 1;
                    i += 1;
                } else if bytes[i] == b' ' {
                    break;
                } else {
                    i += 1; // digits, weight letters (A/B/C/D)
                }
            }
        }

        lexemes.push((lexeme_bytes, npos));
    }

    // Phase 2: Sort lexemes lexicographically — matching PG's tsCompareString
    // which does memcmp(a, b, min(lenA, lenB)) then compares lengths as tiebreaker.
    // This is exactly Rust's default &[u8] Ord (lexicographic byte comparison).
    lexemes.sort_by(|a, b| a.0.cmp(&b.0));

    // Phase 2.5: Merge adjacent duplicates — PG deduplicates identical lexemes
    // and merges their position lists. E.g. 'hello' 'hello' → one entry;
    // 'hello':1 'hello':2 → one entry with 2 positions.
    // Also cap positions at MAXNUMPOS (256) per lexeme, matching PG.
    const MAXNUMPOS: usize = 256;
    let mut deduped: Vec<(Vec<u8>, usize)> = Vec::with_capacity(lexemes.len());
    for (word, npos) in lexemes {
        if let Some(last) = deduped.last_mut() {
            if last.0 == word {
                // Merge: sum position counts (capped at MAXNUMPOS)
                last.1 = (last.1 + npos).min(MAXNUMPOS);
                continue;
            }
        }
        deduped.push((word, npos.min(MAXNUMPOS)));
    }
    let lexemes = deduped;

    // Phase 3: Compute cumulative data_offset in sorted order.
    // PG applies SHORTALIGN to the running offset, NOT to individual lengths.
    let num_lexemes = lexemes.len();
    let mut data_offset = 0usize;
    for (word, npos) in &lexemes {
        data_offset += word.len();

        // PG only writes SHORTALIGN pad + npos + positions when haspos is true.
        // SHORTALIGN rounds the cumulative data_offset up to the next even address.
        if *npos > 0 {
            data_offset = (data_offset + 1) & !1; // SHORTALIGN
            data_offset += 2 + 2 * npos; // npos count + position entries
        }
    }

    // Total: 8 (header) + 4*N (WordEntries) + data_offset
    8 + 4 * num_lexemes + data_offset
}

// ── Tsquery binary format estimation ──────────────────────────────────

/// Estimate PG binary format size for a tsquery datum.
///
/// PG stores tsquery as: [4B varlena][4B nitems][12B × nitems QueryItems]
/// followed by null-terminated operand strings.
///
/// Verified against PG 17:
///   (empty)               → 8     'cat'               → 24
///   'hello' & 'world'     → 56    !'hello'            → 38
///   'a' & 'b' & 'c' & 'd' → 100
pub(crate) fn tsquery_datum_width(s: &str) -> usize {
    if s.is_empty() {
        return 8; // 4 varlena + 4 nitems
    }

    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    let mut noperands = 0usize;
    let mut noperators = 0usize;
    let mut string_bytes = 0usize;

    while i < len {
        while i < len && bytes[i] == b' ' {
            i += 1;
        }
        if i >= len {
            break;
        }

        match bytes[i] {
            b'\'' => {
                i += 1;
                let mut operand_len = 0usize;
                while i < len {
                    if bytes[i] == b'\'' {
                        i += 1;
                        if i < len && bytes[i] == b'\'' {
                            i += 1;
                            operand_len += 1;
                        } else {
                            break;
                        }
                    } else if bytes[i] == b'\\' {
                        i += 1;
                        if i < len {
                            operand_len += 1;
                            i += 1;
                        }
                    } else {
                        operand_len += 1;
                        i += 1;
                    }
                }
                noperands += 1;
                string_bytes += operand_len + 1; // null-terminated

                // Skip optional weight/prefix suffix :A :B :C :D :*
                if i < len && bytes[i] == b':' {
                    i += 1;
                    while i < len
                        && matches!(
                            bytes[i],
                            b'A' | b'B' | b'C' | b'D' | b'*' | b','
                        )
                    {
                        i += 1;
                    }
                }
            }
            b'!' => {
                noperators += 1;
                i += 1;
            }
            b'&' | b'|' => {
                noperators += 1;
                i += 1;
            }
            b'<' => {
                // <-> or <N>
                i += 1;
                while i < len && bytes[i] != b'>' {
                    i += 1;
                }
                if i < len {
                    i += 1;
                }
                noperators += 1;
            }
            b'(' | b')' => {
                i += 1; // grouping only, no item
            }
            _ => {
                // Bare-word operand (unquoted), e.g. hello in `hello::tsquery`
                // Stop at `:` — it begins the optional weight/prefix suffix,
                // which controls flags in the QueryOperand header, not the
                // operand string storage.
                let start = i;
                while i < len
                    && !matches!(
                        bytes[i],
                        b' ' | b'&' | b'|' | b'!' | b'<' | b'(' | b')' | b'\'' | b':'
                    )
                {
                    i += 1;
                }
                let operand_len = i - start;
                // Skip optional weight/prefix suffix :A :B :C :D :*
                if i < len && bytes[i] == b':' {
                    i += 1; // skip ':'
                    while i < len
                        && matches!(
                            bytes[i],
                            b'A' | b'B' | b'C' | b'D' | b'*' | b','
                        )
                    {
                        i += 1;
                    }
                }
                noperands += 1;
                string_bytes += operand_len + 1; // null-terminated
            }
        }
    }

    let nitems = noperands + noperators;
    if nitems == 0 {
        return 8;
    }

    8 + 12 * nitems + string_bytes
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
        Value::Json(s) => s.len() + 4,
        Value::Jsonb(s) => jsonb_datum_width(s),
        Value::Bytes(b) => b.len() + 4,
        Value::Tsvector(s) => tsvector_datum_width(s),
        Value::Tsquery(s) => tsquery_datum_width(s),
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
            Value::Json(s) => s.len() + 4,
            Value::Jsonb(s) => jsonb_datum_width(s),
            Value::Bytes(b) => b.len() + 4,
            Value::Tsvector(s) => tsvector_datum_width(s),
            Value::Tsquery(s) => tsquery_datum_width(s),
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

        // Jsonb -- PG binary format estimation.
        // PG stores jsonb in binary format: pg_column_size('{"a":1}'::jsonb) = 28.
        assert_eq!(
            datum_width(&Value::Jsonb(r#"{"a":1}"#.to_string())),
            28
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

    // ── JSONB binary format tests ──

    /// Verified against PG 17: jsonb binary format sizing.
    #[test]
    fn jsonb_datum_width_pg_parity() {
        assert_eq!(jsonb_datum_width("null"), 12);
        assert_eq!(jsonb_datum_width("true"), 12);
        assert_eq!(jsonb_datum_width("42"), 20);
        assert_eq!(jsonb_datum_width(r#""hello""#), 17);
        assert_eq!(jsonb_datum_width("{}"), 8);
        assert_eq!(jsonb_datum_width(r#"{"a":1}"#), 28);
        assert_eq!(jsonb_datum_width(r#"{"key":"value"}"#), 24);
        assert_eq!(jsonb_datum_width("[1,2,3]"), 44);
        assert_eq!(jsonb_datum_width(r#"{"a":{"b":1}}"#), 44);
    }

    // ── Tsvector binary format tests ──

    /// Verified against PG 17: tsvector binary format sizing.
    #[test]
    fn tsvector_datum_width_pg_parity() {
        assert_eq!(tsvector_datum_width(""), 8);
        // No positions: 4 varlena + 4 nlexemes + 4 WordEntry + 5 lexeme = 17
        assert_eq!(tsvector_datum_width("'hello'"), 17);
        assert_eq!(tsvector_datum_width("'hello':1"), 22);
        assert_eq!(tsvector_datum_width("'hello':1 'world':2"), 36);
        assert_eq!(tsvector_datum_width("'ab':1,2,3"), 22);
        assert_eq!(
            tsvector_datum_width("'a':1 'b':2 'c':3 'd':4 'e':5"),
            58
        );
    }

    /// QG P0 fix: SHORTALIGN must use cumulative data offset, not lexeme length.
    /// Mixed positioned/unpositioned lexemes exercise the cumulative tracking.
    /// Verified against PG 17.
    #[test]
    fn tsvector_datum_width_shortalign_cumulative_offset() {
        // 'abc' 'de':1 → PG=26
        // Sorted: abc(3) < de(2) lexicographically ('a' < 'd').
        // data: abc(3) → offset=3; de(2) → offset=5, SHORTALIGN(5)=6 + 2+2 = 10
        // Total: 8 + 8 + 10 = 26
        assert_eq!(tsvector_datum_width("'abc' 'de':1"), 26);

        // 'a' 'abc':1 → PG=24
        // Header=8, WordEntries=8, data: a(1) + abc(3)=4 → SHORTALIGN(4)=4 + 4 = 8
        // Total: 8 + 8 + 8 = 24
        assert_eq!(tsvector_datum_width("'a' 'abc':1"), 24);

        // 'hello' (no positions) → PG=17
        assert_eq!(tsvector_datum_width("'hello'"), 17);

        // 'hello':1 'world':2 → PG=36
        assert_eq!(tsvector_datum_width("'hello':1 'world':2"), 36);
    }

    /// QG P0 fix (round 5): lexemes must be sorted lexicographically
    /// (matching PG's tsCompareString: memcmp then length tiebreak) before
    /// computing cumulative data_offset. Input order != storage order —
    /// SHORTALIGN padding depends on sorted order.
    /// All values verified against PG 17.
    #[test]
    fn tsvector_datum_width_unsorted_input() {
        // 'world' 'hello':1 → PG=31
        // Sorted: hello(5, pos), world(5, nopos) — 'h' < 'w'.
        // data: hello(5) → SHORTALIGN(5)=6 + 2 + 2 = 10; world(5) → offset=15
        // Total: 8 + 8 + 15 = 31
        assert_eq!(tsvector_datum_width("'world' 'hello':1"), 31);

        // 'zzz':1 'a' → PG=24
        // Sorted: a(1, nopos), zzz(3, pos) — 'a' < 'z'.
        // data: a(1) → offset=1; zzz(3) → offset=4, SHORTALIGN(4)=4 + 2 + 2 = 8
        // Total: 8 + 8 + 8 = 24
        assert_eq!(tsvector_datum_width("'zzz':1 'a'"), 24);

        // Reverse of the above — same result regardless of input order
        assert_eq!(tsvector_datum_width("'a' 'zzz':1"), 24);

        // 'zz' 'aa':1 → sorted: aa(2, pos), zz(2, nopos) — 'a' < 'z'.
        // data: aa(2) → SHORTALIGN(2)=2 + 2 + 2 = 6; zz(2) → offset=8
        // Total: 8 + 8 + 8 = 24
        assert_eq!(tsvector_datum_width("'zz' 'aa':1"), 24);
        assert_eq!(tsvector_datum_width("'aa':1 'zz'"), 24);

        // 'b':1 'a':2 → sorted: a(1, pos), b(1, pos) — 'a' < 'b'.
        // data: a(1) → SHORTALIGN(1)=2 + 2 + 2 = 6; b(1) → offset=7, SHORTALIGN(7)=8 + 2+2=12
        // Total: 8 + 8 + 12 = 28
        assert_eq!(tsvector_datum_width("'b':1 'a':2"), 28);
        assert_eq!(tsvector_datum_width("'a':2 'b':1"), 28);
    }

    /// QG P0 fix (round 6): duplicate lexemes must be merged — PG deduplicates
    /// identical lexemes and combines their position lists into one entry.
    /// Verified against PG 17.
    #[test]
    fn tsvector_datum_width_duplicate_lexeme_dedup() {
        // 'hello' 'hello' → PG=17
        // PG deduplicates to single 'hello' with no positions.
        // 8 + 4 + 5 = 17
        assert_eq!(tsvector_datum_width("'hello' 'hello'"), 17);

        // 'hello':1 'hello':2 → PG=24
        // PG merges to 'hello':1,2 — one entry with 2 positions.
        // 8 + 4 + hello(5) → SHORTALIGN(5)=6 + 2 + 2*2 = 12
        // Total: 8 + 4 + 12 = 24
        assert_eq!(tsvector_datum_width("'hello':1 'hello':2"), 24);

        // 'a':1 'a':2 'a':3 → PG=22
        // PG merges to 'a':1,2,3 — one entry with 3 positions.
        // data: a(1) → offset=1, SHORTALIGN(1)=2 + 2 + 2*3 = 10
        // Total: 8 + 4 + 10 = 22
        assert_eq!(tsvector_datum_width("'a':1 'a':2 'a':3"), 22);

        // Mixed: 'a':1 'b':2 'a':3 → deduped to 'a':1,3 + 'b':2
        // Sorted: a(npos=2), b(npos=1)
        // data: a(1) → offset=1, SHORTALIGN(1)=2 + 2 + 2*2 = 8;
        //       b(1) → offset=9, SHORTALIGN(9)=10 + 2 + 2*1 = 14
        // Total: 8 + 8 + 14 = 30
        assert_eq!(tsvector_datum_width("'a':1 'b':2 'a':3"), 30);
    }

    /// QG P1 fix (round 6): MAXNUMPOS (256) cap on positions per lexeme.
    /// PG caps at 256 positions per lexeme.
    #[test]
    fn tsvector_datum_width_maxnumpos_cap() {
        // Build a tsvector with 300 positions on one lexeme — PG caps at 256.
        // 'w':1,2,3,...,300 → PG stores only 256 positions.
        // 8 + 4 + w(1) → SHORTALIGN(1)=2 + 2 + 2*256 = 516
        // Total: 8 + 4 + 516 = 528
        let positions: Vec<String> = (1..=300).map(|i| i.to_string()).collect();
        let input = format!("'w':{}", positions.join(","));
        assert_eq!(tsvector_datum_width(&input), 528);
    }

    // ── Tsquery binary format tests ──

    /// Verified against PG 17: tsquery binary format sizing.
    #[test]
    fn tsquery_datum_width_pg_parity() {
        assert_eq!(tsquery_datum_width(""), 8);
        assert_eq!(tsquery_datum_width("'cat'"), 24);
        assert_eq!(tsquery_datum_width("'hello' & 'world'"), 56);
        assert_eq!(tsquery_datum_width("!'hello'"), 38);
        assert_eq!(tsquery_datum_width("'a' & 'b' & 'c' & 'd'"), 100);

        // Bare-word (unquoted) tsquery values — verified against PG 17:
        //   SELECT pg_column_size('hello'::tsquery);        → 26
        //   SELECT pg_column_size('cat & dog'::tsquery);    → 52
        assert_eq!(tsquery_datum_width("hello"), 26);
        assert_eq!(tsquery_datum_width("cat & dog"), 52);

        // Bare-word with weight/prefix suffixes — verified against PG 17:
        //   SELECT pg_column_size('hello:*'::tsquery);      → 26
        //   SELECT pg_column_size('hello:A'::tsquery);      → 26
        //   SELECT pg_column_size('hello:*AB'::tsquery);    → 26
        //   SELECT pg_column_size('cat:A & dog:B'::tsquery); → 52
        // The `:...` suffix sets flags in the QueryOperand header but does
        // NOT contribute to the operand string storage.
        assert_eq!(tsquery_datum_width("hello:*"), 26);
        assert_eq!(tsquery_datum_width("hello:A"), 26);
        assert_eq!(tsquery_datum_width("hello:*AB"), 26);
        assert_eq!(tsquery_datum_width("cat:A & dog:B"), 52);
    }

    // ── Array sizing tests (continued) ──

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
