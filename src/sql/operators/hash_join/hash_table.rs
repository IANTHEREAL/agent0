//! Build-side hash table for hash join.
//!
//! Contains the `JoinHashTable` struct and all hashing, equality, and key
//! utility functions used during the build and probe phases of hash join.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use crate::model::{Row, Value};

/// Compute a 64-bit hash for a slice of join-key values.
///
/// Hashing is defined to be compatible with [`join_keys_equal`]:
/// if `join_keys_equal(a, b)` is `true`, then `hash_join_key(a) == hash_join_key(b)`.
#[cfg(test)]
pub fn hash_join_key(values: &[Value]) -> u64 {
    let mut hasher = DefaultHasher::new();
    values.len().hash(&mut hasher);
    for value in values {
        hash_single_value_for_join(&mut hasher, value);
    }
    hasher.finish()
}

pub(super) fn hash_single_value_for_join<H: Hasher>(hasher: &mut H, value: &Value) {
    // Important: hashing must be compatible with `values_equal_for_join`.
    match value {
        Value::Null => {
            // Note: NULL keys are excluded from matching, but hashing remains deterministic.
            0xDEAD_BEEF_CAFE_BABEu64.hash(hasher);
        }
        Value::Int32(n) => {
            // Int32/Int64 compare as i64 in `values_equal_for_join`.
            1u8.hash(hasher);
            i64::from(*n).hash(hasher);
        }
        Value::Int64(n) => {
            1u8.hash(hasher);
            n.hash(hasher);
        }
        Value::Numeric(d) => {
            2u8.hash(hasher);
            let normalized = d.normalize();
            let unpacked = normalized.unpack();
            unpacked.lo.hash(hasher);
            unpacked.mid.hash(hasher);
            unpacked.hi.hash(hasher);
            unpacked.negative.hash(hasher);
            unpacked.scale.hash(hasher);
        }
        Value::Float64(f) => {
            3u8.hash(hasher);
            let f = *f;
            if f.is_nan() {
                // PostgreSQL treats NaN = NaN as true for equality, so all NaNs must hash equal.
                u64::MAX.hash(hasher);
            } else if f == 0.0 {
                // -0.0 == 0.0, so they must hash equal as well.
                0.0f64.to_bits().hash(hasher);
            } else {
                f.to_bits().hash(hasher);
            }
        }
        other => {
            // Default: include a discriminant so different Value variants do not collide.
            std::mem::discriminant(other).hash(hasher);
            match other {
                Value::Boolean(b) => b.hash(hasher),
                Value::Text(s) => s.hash(hasher),
                Value::Bytes(b) => b.hash(hasher),
                Value::Timestamp(ts) => ts.hash(hasher),
                Value::Interval(iv) => {
                    iv.months.hash(hasher);
                    iv.millis.hash(hasher);
                }
                Value::Uuid(bytes) => bytes.hash(hasher),
                Value::Array(arr) => {
                    arr.len().hash(hasher);
                    for elem in arr {
                        hash_single_value_for_join(hasher, elem);
                    }
                }
                Value::Vector(vec) => {
                    vec.len().hash(hasher);
                    for f in vec {
                        f.to_bits().hash(hasher);
                    }
                }
                Value::Json(s) | Value::Jsonb(s) => s.hash(hasher),
                Value::Time(t) => t.hash(hasher),
                Value::Date(d) => d.hash(hasher),
                Value::Tsvector(s) | Value::Tsquery(s) => s.hash(hasher),
                Value::Int32(_)
                | Value::Int64(_)
                | Value::Numeric(_)
                | Value::Float64(_)
                | Value::Null => {
                    // Covered above
                }
            }
        }
    }
}

/// Compare two join keys for equality with SQL semantics.
///
/// - `NULL != NULL` (returns `false`)
/// - `NaN == NaN` (returns `true`, PostgreSQL-like)
/// - `Int32` and `Int64` are compared as `i64`
/// - `Numeric` is normalized before compare (`1.0 == 1.00`)
#[cfg(test)]
pub fn join_keys_equal(left: &[Value], right: &[Value]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right.iter())
        .all(|(l, r)| values_equal_for_join(l, r))
}

pub(super) fn values_equal_for_join(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Null, _) | (_, Value::Null) => false,
        (Value::Float64(a), Value::Float64(b)) => a == b || (a.is_nan() && b.is_nan()),
        (Value::Int32(a), Value::Int64(b)) => i64::from(*a) == *b,
        (Value::Int64(a), Value::Int32(b)) => *a == i64::from(*b),
        (Value::Numeric(a), Value::Numeric(b)) => a.normalize() == b.normalize(),
        _ => left == right,
    }
}

pub(crate) fn row_key_has_null_for_join(row: &Row, key_indices: &[usize]) -> bool {
    key_indices
        .iter()
        .any(|&idx| matches!(row.values.get(idx), Some(Value::Null) | None))
}

pub(crate) fn hash_row_key_for_join(row: &Row, key_indices: &[usize]) -> u64 {
    let mut hasher = DefaultHasher::new();
    key_indices.len().hash(&mut hasher);
    for &idx in key_indices {
        let value = row.values.get(idx).unwrap_or(&Value::Null);
        hash_single_value_for_join(&mut hasher, value);
    }
    hasher.finish()
}

pub(crate) fn row_keys_equal_for_join(
    build_row: &Row,
    build_key_indices: &[usize],
    probe_row: &Row,
    probe_key_indices: &[usize],
) -> bool {
    if build_key_indices.len() != probe_key_indices.len() {
        return false;
    }
    for (b_idx, p_idx) in build_key_indices
        .iter()
        .copied()
        .zip(probe_key_indices.iter().copied())
    {
        let b_val = build_row.values.get(b_idx).unwrap_or(&Value::Null);
        let p_val = probe_row.values.get(p_idx).unwrap_or(&Value::Null);
        if !values_equal_for_join(b_val, p_val) {
            return false;
        }
    }
    true
}

#[derive(Debug, Default)]
pub(super) struct HashBucket {
    pub(super) rows: Vec<Row>,
    pub(super) global_indices: Vec<usize>,
}

/// Build-side hash table for hash join.
#[derive(Debug)]
pub struct JoinHashTable {
    pub(super) buckets: HashMap<u64, HashBucket>,
    key_indices: Vec<usize>,
    row_count: usize,
    memory_bytes: usize,
    pub(super) null_key_rows: Vec<Row>,
    pub(super) null_key_start_index: usize,
}

impl Default for JoinHashTable {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl JoinHashTable {
    /// Create an empty hash table for a join key defined by `key_indices`.
    pub fn new(key_indices: Vec<usize>) -> Self {
        Self {
            buckets: HashMap::new(),
            key_indices,
            row_count: 0,
            memory_bytes: 0,
            null_key_rows: Vec::new(),
            null_key_start_index: 0,
        }
    }

    /// Create an empty hash table with capacity hint.
    pub fn with_capacity(key_indices: Vec<usize>, estimated_rows: usize) -> Self {
        Self {
            buckets: HashMap::with_capacity((estimated_rows as f64 * 1.4) as usize),
            key_indices,
            row_count: 0,
            memory_bytes: 0,
            null_key_rows: Vec::new(),
            null_key_start_index: 0,
        }
    }

    /// Insert a build-side row into the hash table.
    ///
    /// Returns an estimated memory delta for observability.
    pub fn insert(&mut self, row: Row) -> usize {
        let mem_delta = Self::estimate_row_size(&row);

        if self.row_key_has_null(&row) {
            self.null_key_rows.push(row);
            self.memory_bytes += mem_delta;
            return mem_delta;
        }

        let hash = self.hash_row_key(&row);
        let global_idx = self.row_count;
        let bucket = self.buckets.entry(hash).or_default();
        bucket.rows.push(row);
        bucket.global_indices.push(global_idx);

        self.row_count += 1;
        self.memory_bytes += mem_delta;
        mem_delta
    }

    /// Finalize the hash table after all inserts.
    pub fn finalize(&mut self) {
        self.null_key_start_index = self.row_count;
    }

    /// Total number of build rows tracked by the table (including NULL-key rows).
    pub fn total_row_count(&self) -> usize {
        self.row_count + self.null_key_rows.len()
    }

    /// Estimated memory usage (bytes).
    pub fn memory_bytes(&self) -> usize {
        self.memory_bytes
    }

    fn hash_row_key(&self, row: &Row) -> u64 {
        hash_row_key_for_join(row, &self.key_indices)
    }

    fn row_key_has_null(&self, row: &Row) -> bool {
        row_key_has_null_for_join(row, &self.key_indices)
    }

    #[cfg(test)]
    pub(crate) fn row_key_equals_values(&self, row: &Row, probe_key: &[Value]) -> bool {
        if self.key_indices.len() != probe_key.len() {
            return false;
        }
        for (pos, &idx) in self.key_indices.iter().enumerate() {
            let v = row.values.get(idx).unwrap_or(&Value::Null);
            if !values_equal_for_join(v, &probe_key[pos]) {
                return false;
            }
        }
        true
    }

    fn estimate_row_size(row: &Row) -> usize {
        std::mem::size_of::<Row>()
            + row
                .values
                .iter()
                .map(|v| match v {
                    Value::Text(s) => s.len() + 24,
                    Value::Bytes(b) => b.len() + 24,
                    Value::Array(a) => a.len() * 16 + 24,
                    Value::Vector(v) => v.len() * 8 + 24,
                    Value::Json(s) | Value::Jsonb(s) => s.len() + 24,
                    _ => 16,
                })
                .sum::<usize>()
    }

    pub(super) fn into_unmatched_parts(self) -> (Vec<HashBucket>, Vec<Row>, usize) {
        (
            self.buckets.into_values().collect(),
            self.null_key_rows,
            self.null_key_start_index,
        )
    }

    pub fn bucket_by_hash(&self, hash: u64) -> Option<(&[Row], &[usize])> {
        self.buckets
            .get(&hash)
            .map(|b| (b.rows.as_slice(), b.global_indices.as_slice()))
    }

    #[cfg(test)]
    pub fn all_rows_with_indices(&self) -> impl Iterator<Item = (usize, &Row)> + '_ {
        self.buckets
            .values()
            .flat_map(|b| b.global_indices.iter().copied().zip(b.rows.iter()))
            .chain(
                self.null_key_rows
                    .iter()
                    .enumerate()
                    .map(move |(i, r)| (self.null_key_start_index + i, r)),
            )
    }
}
