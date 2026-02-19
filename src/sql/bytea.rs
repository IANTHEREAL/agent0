//! PostgreSQL-compatible `BYTEA` operations used by the SQL expression evaluator.

use anyhow::{anyhow, Result};

/// Returns the bit value (0/1) at `bit_index` in `bytes`.
///
/// Semantics match PostgreSQL `get_bit(bytea, int)`:
/// - `bit_index` is 0-based.
/// - Bits are addressed in big-endian order within each byte (MSB first).
#[cfg(test)]
pub(crate) fn get_bit(bytes: &[u8], bit_index: i64) -> Result<i32> {
    let bit_index: usize = bit_index
        .try_into()
        .map_err(|_| anyhow!("get_bit: bit index {} out of range", bit_index))?;

    let byte_index = bit_index / 8;
    if byte_index >= bytes.len() {
        return Err(anyhow!("get_bit: bit index {} out of range", bit_index));
    }
    let bit_offset = 7 - (bit_index % 8);
    Ok(((bytes[byte_index] >> bit_offset) & 1) as i32)
}

/// Sets the bit at `bit_index` in `bytes` and returns the modified `Vec<u8>`.
///
/// Semantics match PostgreSQL `set_bit(bytea, int, int)`:
/// - `bit_index` is 0-based.
/// - Bits are addressed in big-endian order within each byte (MSB first).
/// - `new_value` must be 0 or 1.
/// - The input length is not extended; out-of-range indices raise an error.
#[cfg(test)]
pub(crate) fn set_bit(mut bytes: Vec<u8>, bit_index: i64, new_value: i64) -> Result<Vec<u8>> {
    if new_value != 0 && new_value != 1 {
        return Err(anyhow!("set_bit: new value must be 0 or 1"));
    }

    let bit_index: usize = bit_index
        .try_into()
        .map_err(|_| anyhow!("set_bit: bit index {} out of range", bit_index))?;

    let byte_index = bit_index / 8;
    if byte_index >= bytes.len() {
        return Err(anyhow!("set_bit: bit index {} out of range", bit_index));
    }
    let bit_offset = 7 - (bit_index % 8);
    let mask = 1u8 << bit_offset;

    if new_value == 1 {
        bytes[byte_index] |= mask;
    } else {
        bytes[byte_index] &= !mask;
    }
    Ok(bytes)
}

/// PostgreSQL `int8send(bigint)`: encode `value` as 8 bytes, big-endian.
#[cfg(test)]
pub(crate) fn int8send(value: i64) -> Vec<u8> {
    Vec::from(value.to_be_bytes())
}

/// PostgreSQL `int4send(int)`: encode `value` as 4 bytes, big-endian.
#[cfg(test)]
pub(crate) fn int4send(value: i32) -> Vec<u8> {
    Vec::from(value.to_be_bytes())
}

/// PostgreSQL `uuid_send(uuid)`: encode UUID as 16 raw bytes.
#[cfg(test)]
pub(crate) fn uuid_send(value: [u8; 16]) -> Vec<u8> {
    Vec::from(value)
}

/// PostgreSQL `encode(data, 'escape')`: convert `BYTEA` to the legacy escape format.
///
/// Behavior matches PostgreSQL `encode(bytea, 'escape')`:
/// - Only `\0` (null byte), `\\` (backslash), and high-bit bytes (`0x80..=0xff`)
///   are escaped. Everything else (`0x01..=0x7f` except `\\`) is emitted as-is.
///
/// Note: this differs from `byteaout()` which also escapes control chars
/// `0x01..=0x1f` and `0x7f`. The `encode(escape)` function is intentionally
/// more permissive, matching PostgreSQL's `src/backend/utils/adt/encode.c`.
pub(crate) fn encode_escape(bytes: &[u8]) -> String {
    // Fast path: no null bytes, no backslashes, no high-bit bytes.
    if bytes.iter().all(|&b| b != 0x00 && b != b'\\' && b < 0x80) {
        // SAFETY: bytes 0x01..=0x7f are valid UTF-8 (ASCII range).
        return unsafe { String::from_utf8_unchecked(bytes.to_vec()) };
    }

    let mut out = Vec::with_capacity(bytes.len().saturating_mul(4));
    for &b in bytes {
        match b {
            b'\\' => out.extend_from_slice(b"\\\\"),
            0x00 | 0x80..=0xff => {
                out.push(b'\\');
                out.push(b'0' + ((b >> 6) & 0x07));
                out.push(b'0' + ((b >> 3) & 0x07));
                out.push(b'0' + (b & 0x07));
            }
            _ => out.push(b),
        }
    }
    // SAFETY: output contains only 0x01..=0x7f bytes and ASCII digits/backslashes.
    unsafe { String::from_utf8_unchecked(out) }
}

/// PostgreSQL `decode(string, 'escape')`: parse the legacy escape format into raw bytes.
pub(crate) fn decode_escape(s: &str) -> Result<Vec<u8>> {
    let input = s.as_bytes();
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0usize;

    while i < input.len() {
        if input[i] != b'\\' {
            out.push(input[i]);
            i += 1;
            continue;
        }

        // Backslash escape.
        if i + 1 >= input.len() {
            out.push(b'\\');
            break;
        }

        match input[i + 1] {
            b'\\' => {
                out.push(b'\\');
                i += 2;
            }
            b'0'..=b'9' => {
                // Octal escape sequence. PostgreSQL produces 3 digits; accept 1-3 digits for
                // leniency (matching many client expectations).
                let mut oct = 0u16;
                let mut digits = 0u8;
                let mut j = i + 1;
                while j < input.len() && digits < 3 {
                    let d = input[j];
                    if !(b'0'..=b'9').contains(&d) {
                        break;
                    }
                    if d > b'7' {
                        return Err(anyhow!("invalid escape sequence"));
                    }
                    oct = (oct << 3) | u16::from(d - b'0');
                    digits += 1;
                    j += 1;
                }
                if digits == 0 || oct > 0xff {
                    return Err(anyhow!("invalid escape sequence"));
                }
                out.push(oct as u8);
                i = j;
            }
            _ => {
                // Unknown escape: treat the backslash as a literal (best-effort decoding).
                out.push(b'\\');
                i += 1;
            }
        }
    }

    Ok(out)
}

/// PostgreSQL `substring(bytea from start [for count])`.
///
/// `start` is 1-based. Values <= 0 behave like 1. Negative/zero lengths are treated as 0.
///
/// This function takes ownership of `bytes` so callers can avoid extra allocations.
pub(crate) fn substring(mut bytes: Vec<u8>, start: i64, count: Option<i64>) -> Vec<u8> {
    let start_idx = if start <= 1 {
        0usize
    } else {
        usize::try_from(start - 1).unwrap_or(usize::MAX)
    };
    if start_idx >= bytes.len() {
        bytes.clear();
        return bytes;
    }

    let mut end = bytes.len();
    if let Some(count) = count {
        let count = count.max(0) as usize;
        end = start_idx.saturating_add(count).min(bytes.len());
    }

    bytes.truncate(end);
    if start_idx > 0 {
        bytes.drain(..start_idx);
    }
    bytes
}

/// PostgreSQL `overlay(bytea placing bytea from start [for count])`.
///
/// `start` is 1-based. Values <= 0 return the input unchanged.
/// When `count` is not provided, it defaults to `placing.len()`.
///
/// This function mutates `base` in-place where possible to minimize allocations/copies.
pub(crate) fn overlay(
    mut base: Vec<u8>,
    placing: &[u8],
    start: i64,
    count: Option<i64>,
) -> Vec<u8> {
    if start <= 0 {
        return base;
    }

    let start_idx = usize::try_from(start - 1).unwrap_or(usize::MAX);
    let replace_len = count
        .map(|n| usize::try_from(n.max(0)).unwrap_or(usize::MAX))
        .unwrap_or_else(|| placing.len());

    let original_len = base.len();
    let prefix_len = original_len.min(start_idx);
    let suffix_start = original_len.min(start_idx.saturating_add(replace_len));
    let suffix_len = original_len - suffix_start;

    let new_len = prefix_len + placing.len() + suffix_len;
    let removed_len = suffix_start - prefix_len;

    // Fast-path: pure replacement with equal lengths (common in UUIDv7 construction).
    if placing.len() == removed_len && prefix_len < original_len {
        base[prefix_len..suffix_start].copy_from_slice(placing);
        return base;
    }

    if new_len > original_len {
        base.resize(new_len, 0);
    }

    let dest_suffix_start = prefix_len + placing.len();
    base.copy_within(suffix_start..original_len, dest_suffix_start);

    if new_len < original_len {
        base.truncate(new_len);
    }

    base[prefix_len..prefix_len + placing.len()].copy_from_slice(placing);
    base
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_bit_big_endian() {
        assert_eq!(get_bit(&[0x80], 0).unwrap(), 1);
        assert_eq!(get_bit(&[0x80], 7).unwrap(), 0);
        assert_eq!(get_bit(&[0x01], 7).unwrap(), 1);
    }

    #[test]
    fn set_bit_big_endian() {
        assert_eq!(set_bit(vec![0x00], 0, 1).unwrap(), vec![0x80]);
        assert_eq!(set_bit(vec![0x00], 7, 1).unwrap(), vec![0x01]);
        assert_eq!(set_bit(vec![0xff], 0, 0).unwrap(), vec![0x7f]);
    }

    #[test]
    fn int_send_endianness() {
        assert_eq!(int8send(0x0102030405060708), vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(int4send(0x01020304), vec![1, 2, 3, 4]);
    }

    #[test]
    fn uuid_send_length() {
        let uuid = uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        assert_eq!(uuid_send(*uuid.as_bytes()).len(), 16);
    }

    #[test]
    fn substring_bytea_basic() {
        assert_eq!(substring(vec![1, 2, 3, 4], 1, None), vec![1, 2, 3, 4]);
        assert_eq!(substring(vec![1, 2, 3, 4], 3, None), vec![3, 4]);
        assert_eq!(substring(vec![1, 2, 3, 4], 3, Some(1)), vec![3]);
        assert_eq!(substring(vec![1, 2, 3, 4], 10, None), Vec::<u8>::new());
    }

    #[test]
    fn overlay_bytea_replace_and_insert() {
        // Replace 2 bytes starting at position 2.
        assert_eq!(
            overlay(vec![0x00, 0x11, 0x22, 0x33], &[0xaa, 0xbb], 2, Some(2)),
            vec![0x00, 0xaa, 0xbb, 0x33]
        );

        // Insert past the end appends.
        assert_eq!(
            overlay(vec![0x00, 0x11], &[0xaa], 10, Some(1)),
            vec![0x00, 0x11, 0xaa]
        );
    }

    #[test]
    fn escape_encode_decode_roundtrip() {
        let bytes = vec![b'H', b'i', b'\\', 0, 0xff];
        let encoded = encode_escape(&bytes);
        let decoded = decode_escape(&encoded).unwrap();
        assert_eq!(decoded, bytes);
    }

    #[test]
    fn escape_encode_matches_postgres_conventions() {
        // PG encode(escape) only escapes \0, \\, and 0x80-0xff.
        // Control chars 0x01-0x1f and 0x7f are passed through as-is.
        let bytes = vec![b'\\', 0, 0x1f, b' ', 0x7f, 0xff];
        let encoded = encode_escape(&bytes);
        // Expected: \\ \000 <raw 0x1f> <space> <raw 0x7f> \377
        assert_eq!(encoded, "\\\\\\000\x1f \x7f\\377");
        assert_eq!(encoded.len(), 13);
    }

    #[test]
    fn escape_decode_rejects_invalid_octal() {
        assert!(decode_escape(r"\8").is_err());
        assert!(decode_escape(r"\999").is_err());
    }
}
