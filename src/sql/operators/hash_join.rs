//! Hash Join operator for equi-joins.
//!
//! This module implements a classic hash join:
//! - **Build** phase: materialize the build side into an in-memory hash table
//! - **Probe** phase: stream the probe side and lookup matching build rows
//!
//! Notes on correctness:
//! - SQL semantics: `NULL` never equals anything (including `NULL`). Rows with `NULL` in any join
//!   key column are stored separately and never match during probe.
//! - Output column order is always `left + right`, regardless of which side is chosen as build.
//!
//! Notes on performance:
//! - Join keys are hashed/compared directly from rows by column indices to avoid allocating
//!   intermediate key vectors on the hot path.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use sqlparser::ast::Expr;

use super::{BoxedOperator, ExecutionContext, PhysicalOperator};
use crate::sql::expr::eval_expr;
use crate::types::{ColumnDef, Row, TableSchema, Value};

/// Compute a 64-bit hash for a slice of join-key values.
///
/// Hashing is defined to be compatible with [`join_keys_equal`]:
/// if `join_keys_equal(a, b)` is `true`, then `hash_join_key(a) == hash_join_key(b)`.
#[allow(dead_code)] // hash join operator framework
pub fn hash_join_key(values: &[Value]) -> u64 {
    let mut hasher = DefaultHasher::new();
    values.len().hash(&mut hasher);
    for value in values {
        hash_single_value_for_join(&mut hasher, value);
    }
    hasher.finish()
}

fn hash_single_value_for_join<H: Hasher>(hasher: &mut H, value: &Value) {
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
#[allow(dead_code)] // hash join operator framework
pub fn join_keys_equal(left: &[Value], right: &[Value]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right.iter())
        .all(|(l, r)| values_equal_for_join(l, r))
}

fn values_equal_for_join(left: &Value, right: &Value) -> bool {
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
struct HashBucket {
    rows: Vec<Row>,
    global_indices: Vec<usize>,
}

/// Configuration for hash join planning/execution.
#[derive(Debug, Clone)]
pub struct HashJoinConfig {
    /// Maximum memory for the build-side hash table (bytes).
    pub max_memory_bytes: usize,
    /// Minimum total input rows required to prefer hash join over nested loop.
    pub min_rows_threshold: usize,
}

impl Default for HashJoinConfig {
    fn default() -> Self {
        Self {
            max_memory_bytes: 256 * 1024 * 1024,
            min_rows_threshold: 100,
        }
    }
}

/// Build-side hash table for hash join.
#[derive(Debug)]
pub struct JoinHashTable {
    buckets: HashMap<u64, HashBucket>,
    key_indices: Vec<usize>,
    row_count: usize,
    memory_bytes: usize,
    null_key_rows: Vec<Row>,
    null_key_start_index: usize,
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

    #[allow(dead_code)] // hash join operator framework
    fn row_key_equals_values(&self, row: &Row, probe_key: &[Value]) -> bool {
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

    fn into_unmatched_parts(self) -> (Vec<HashBucket>, Vec<Row>, usize) {
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

    #[allow(dead_code)] // hash join operator framework
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

/// Join type for [`HashJoinOperator`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashJoinType {
    Inner,
    Left,
    Right,
    Full,
}

#[derive(Debug)]
enum HashJoinState {
    Created,
    Probing {
        hash_table: JoinHashTable,
        build_matched: Option<Vec<bool>>,
        current_probe_row: Option<Row>,
        current_probe_hash: Option<u64>,
        current_bucket_pos: usize,
        probe_had_match: bool,
    },
    EmittingUnmatchedBuild {
        buckets: Vec<HashBucket>,
        bucket_idx: usize,
        bucket_row_idx: usize,
        null_key_rows: Vec<Row>,
        null_key_pos: usize,
        null_key_start_index: usize,
        build_matched: Vec<bool>,
    },
    Exhausted,
}

/// Hash Join operator implementing the Volcano iterator model.
#[derive(Debug)]
pub struct HashJoinOperator {
    build_child: BoxedOperator,
    probe_child: BoxedOperator,
    #[allow(dead_code)] // hash join operator framework
    join_type: HashJoinType,
    left_is_build: bool,
    build_key_indices: Vec<usize>,
    probe_key_indices: Vec<usize>,
    build_outer: bool,
    probe_outer: bool,
    filter: Option<Expr>,
    output_schema: TableSchema,
    config: HashJoinConfig,
    state: HashJoinState,
}

impl HashJoinOperator {
    /// Create a new hash join operator.
    ///
    /// `left_key_indices` and `right_key_indices` are column indices in the **original**
    /// left/right inputs that form the join key, in order.
    ///
    /// `left_is_build` selects which input is the build side (typically the smaller relation).
    pub fn new(
        left_child: BoxedOperator,
        right_child: BoxedOperator,
        join_type: HashJoinType,
        left_key_indices: Vec<usize>,
        right_key_indices: Vec<usize>,
        left_is_build: bool,
        filter: Option<Expr>,
        config: HashJoinConfig,
    ) -> Self {
        let mut columns: Vec<ColumnDef> = Vec::new();
        for col in &left_child.schema().columns {
            columns.push(ColumnDef {
                name: col.name.clone(),
                data_type: col.data_type.clone(),
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            });
        }
        for col in &right_child.schema().columns {
            columns.push(ColumnDef {
                name: col.name.clone(),
                data_type: col.data_type.clone(),
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            });
        }

        let output_schema = TableSchema {
            name: "hash_join".to_string(),
            table_id: 0,
            columns,
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };

        let (build_child, probe_child, build_key_indices, probe_key_indices) = if left_is_build {
            (left_child, right_child, left_key_indices, right_key_indices)
        } else {
            (right_child, left_child, right_key_indices, left_key_indices)
        };

        let left_outer = matches!(join_type, HashJoinType::Left | HashJoinType::Full);
        let right_outer = matches!(join_type, HashJoinType::Right | HashJoinType::Full);
        let (build_outer, probe_outer) = if left_is_build {
            (left_outer, right_outer)
        } else {
            (right_outer, left_outer)
        };

        Self {
            build_child,
            probe_child,
            join_type,
            left_is_build,
            build_key_indices,
            probe_key_indices,
            build_outer,
            probe_outer,
            filter,
            output_schema,
            config,
            state: HashJoinState::Created,
        }
    }

    pub fn with_output_schema(mut self, schema: TableSchema) -> Self {
        self.output_schema = schema;
        self
    }

    fn make_output_row(&self, probe_row: &Row, build_row: &Row) -> Row {
        let left_len = if self.left_is_build {
            build_row.values.len()
        } else {
            probe_row.values.len()
        };
        let right_len = if self.left_is_build {
            probe_row.values.len()
        } else {
            build_row.values.len()
        };

        let mut values = Vec::with_capacity(left_len + right_len);
        if self.left_is_build {
            values.extend(build_row.values.iter().cloned());
            values.extend(probe_row.values.iter().cloned());
        } else {
            values.extend(probe_row.values.iter().cloned());
            values.extend(build_row.values.iter().cloned());
        }
        Row::new(values)
    }

    fn make_probe_only_row(&self, probe_row: &Row) -> Row {
        let null_count = self.build_child.schema().columns.len();
        let total_len = self.output_schema.columns.len();
        let mut values = Vec::with_capacity(total_len);
        if self.left_is_build {
            values.extend(std::iter::repeat(Value::Null).take(null_count));
            values.extend(probe_row.values.iter().cloned());
        } else {
            values.extend(probe_row.values.iter().cloned());
            values.extend(std::iter::repeat(Value::Null).take(null_count));
        }
        Row::new(values)
    }

    fn make_build_only_row(&self, build_row: &Row) -> Row {
        let null_count = self.probe_child.schema().columns.len();
        let total_len = self.output_schema.columns.len();
        let mut values = Vec::with_capacity(total_len);
        if self.left_is_build {
            values.extend(build_row.values.iter().cloned());
            values.extend(std::iter::repeat(Value::Null).take(null_count));
        } else {
            values.extend(std::iter::repeat(Value::Null).take(null_count));
            values.extend(build_row.values.iter().cloned());
        }
        Row::new(values)
    }

    fn check_filter(&self, row: &Row) -> Result<bool> {
        match &self.filter {
            None => Ok(true),
            Some(expr) => match eval_expr(expr, Some(row), Some(&self.output_schema))? {
                Value::Boolean(b) => Ok(b),
                Value::Null => Ok(false),
                _ => Err(anyhow!("JOIN filter must be boolean")),
            },
        }
    }

    fn row_key_has_null(row: &Row, key_indices: &[usize]) -> bool {
        row_key_has_null_for_join(row, key_indices)
    }

    fn hash_row_key(row: &Row, key_indices: &[usize]) -> u64 {
        hash_row_key_for_join(row, key_indices)
    }

    fn row_keys_equal(
        build_row: &Row,
        build_key_indices: &[usize],
        probe_row: &Row,
        probe_key_indices: &[usize],
    ) -> bool {
        row_keys_equal_for_join(build_row, build_key_indices, probe_row, probe_key_indices)
    }
}

#[allow(dead_code)] // hash join operator framework
#[async_trait]
impl PhysicalOperator for HashJoinOperator {
    fn schema(&self) -> &TableSchema {
        &self.output_schema
    }

    async fn open(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.build_child.open(ctx).await?;
        self.probe_child.open(ctx).await?;

        let mut hash_table = JoinHashTable::with_capacity(
            self.build_key_indices.clone(),
            self.build_child.estimated_rows().unwrap_or(0),
        );

        while let Some(row) = self.build_child.next(ctx).await? {
            hash_table.insert(row);
            if hash_table.memory_bytes() > self.config.max_memory_bytes {
                tracing::warn!(
                    "Hash join exceeded memory limit: {} > {}",
                    hash_table.memory_bytes(),
                    self.config.max_memory_bytes
                );
            }
        }
        hash_table.finalize();

        let build_matched = if self.build_outer {
            Some(vec![false; hash_table.total_row_count()])
        } else {
            None
        };

        self.state = HashJoinState::Probing {
            hash_table,
            build_matched,
            current_probe_row: None,
            current_probe_hash: None,
            current_bucket_pos: 0,
            probe_had_match: false,
        };

        Ok(())
    }

    async fn next(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>> {
        let left_is_build = self.left_is_build;
        let build_outer = self.build_outer;
        let probe_outer = self.probe_outer;
        let build_key_indices = self.build_key_indices.as_slice();
        let probe_key_indices = self.probe_key_indices.as_slice();
        let build_col_count = self.build_child.schema().columns.len();
        let probe_col_count = self.probe_child.schema().columns.len();
        let filter = self.filter.as_ref();
        let output_schema = &self.output_schema;

        let check_filter = |row: &Row| -> Result<bool> {
            match filter {
                None => Ok(true),
                Some(expr) => match eval_expr(expr, Some(row), Some(output_schema))? {
                    Value::Boolean(b) => Ok(b),
                    Value::Null => Ok(false),
                    _ => Err(anyhow!("JOIN filter must be boolean")),
                },
            }
        };

        let make_output_row = |probe_row: &Row, build_row: &Row| -> Row {
            let mut values = Vec::with_capacity(probe_row.values.len() + build_row.values.len());
            if left_is_build {
                values.extend(build_row.values.iter().cloned());
                values.extend(probe_row.values.iter().cloned());
            } else {
                values.extend(probe_row.values.iter().cloned());
                values.extend(build_row.values.iter().cloned());
            }
            Row::new(values)
        };

        let make_probe_only_row = |probe_row: &Row| -> Row {
            let total_len = build_col_count + probe_col_count;
            let mut values = Vec::with_capacity(total_len);
            if left_is_build {
                values.extend(std::iter::repeat(Value::Null).take(build_col_count));
                values.extend(probe_row.values.iter().cloned());
            } else {
                values.extend(probe_row.values.iter().cloned());
                values.extend(std::iter::repeat(Value::Null).take(build_col_count));
            }
            Row::new(values)
        };

        let make_build_only_row = |build_row: &Row| -> Row {
            let total_len = build_col_count + probe_col_count;
            let mut values = Vec::with_capacity(total_len);
            if left_is_build {
                values.extend(build_row.values.iter().cloned());
                values.extend(std::iter::repeat(Value::Null).take(probe_col_count));
            } else {
                values.extend(std::iter::repeat(Value::Null).take(probe_col_count));
                values.extend(build_row.values.iter().cloned());
            }
            Row::new(values)
        };

        loop {
            match &mut self.state {
                HashJoinState::Created => return Err(anyhow!("HashJoinOperator not opened")),
                HashJoinState::Probing {
                    hash_table,
                    build_matched,
                    current_probe_row,
                    current_probe_hash,
                    current_bucket_pos,
                    probe_had_match,
                } => {
                    if let Some(probe_row) = current_probe_row.as_ref() {
                        if let Some(hash) = *current_probe_hash {
                            if let Some((bucket_rows, bucket_indices)) =
                                hash_table.bucket_by_hash(hash)
                            {
                                while *current_bucket_pos < bucket_rows.len() {
                                    let pos = *current_bucket_pos;
                                    *current_bucket_pos += 1;

                                    let build_row = &bucket_rows[pos];
                                    if !row_keys_equal_for_join(
                                        build_row,
                                        build_key_indices,
                                        probe_row,
                                        probe_key_indices,
                                    ) {
                                        continue;
                                    }

                                    let out = make_output_row(probe_row, build_row);
                                    if !check_filter(&out)? {
                                        continue;
                                    }

                                    *probe_had_match = true;
                                    if build_outer {
                                        if let Some(bm) = build_matched.as_mut() {
                                            let idx = bucket_indices[pos];
                                            if idx < bm.len() {
                                                bm[idx] = true;
                                            }
                                        }
                                    }
                                    return Ok(Some(out));
                                }
                            }
                        }

                        // No more matches for current probe row.
                        let need_probe_unmatched = probe_outer && !*probe_had_match;
                        let probe_row = current_probe_row.take().unwrap();
                        *current_probe_hash = None;
                        *current_bucket_pos = 0;
                        *probe_had_match = false;

                        if need_probe_unmatched {
                            return Ok(Some(make_probe_only_row(&probe_row)));
                        }
                    }

                    // Fetch next probe row.
                    match self.probe_child.next(ctx).await? {
                        Some(row) => {
                            let has_null = row_key_has_null_for_join(&row, probe_key_indices);
                            *current_probe_hash = if has_null {
                                None
                            } else {
                                Some(hash_row_key_for_join(&row, probe_key_indices))
                            };
                            *current_bucket_pos = 0;
                            *probe_had_match = false;
                            *current_probe_row = Some(row);
                            continue;
                        }
                        None => {
                            // Probe exhausted.
                            if build_outer {
                                let build_matched = build_matched
                                    .take()
                                    .ok_or_else(|| anyhow!("Missing build_matched bitmap"))?;
                                let (buckets, null_key_rows, null_key_start_index) =
                                    std::mem::take(hash_table).into_unmatched_parts();
                                self.state = HashJoinState::EmittingUnmatchedBuild {
                                    buckets,
                                    bucket_idx: 0,
                                    bucket_row_idx: 0,
                                    null_key_rows,
                                    null_key_pos: 0,
                                    null_key_start_index,
                                    build_matched,
                                };
                                continue;
                            }
                            self.state = HashJoinState::Exhausted;
                            return Ok(None);
                        }
                    }
                }
                HashJoinState::EmittingUnmatchedBuild {
                    buckets,
                    bucket_idx,
                    bucket_row_idx,
                    null_key_rows,
                    null_key_pos,
                    null_key_start_index,
                    build_matched,
                } => {
                    while *bucket_idx < buckets.len() {
                        let bucket = &buckets[*bucket_idx];
                        while *bucket_row_idx < bucket.rows.len() {
                            let pos = *bucket_row_idx;
                            *bucket_row_idx += 1;
                            let global_idx = bucket.global_indices[pos];
                            if build_matched.get(global_idx).copied().unwrap_or(false) {
                                continue;
                            }
                            let row = &bucket.rows[pos];
                            return Ok(Some(make_build_only_row(row)));
                        }
                        *bucket_idx += 1;
                        *bucket_row_idx = 0;
                    }

                    while *null_key_pos < null_key_rows.len() {
                        let pos = *null_key_pos;
                        *null_key_pos += 1;
                        let global_idx = null_key_start_index.saturating_add(pos);
                        if build_matched.get(global_idx).copied().unwrap_or(false) {
                            continue;
                        }
                        let row = &null_key_rows[pos];
                        return Ok(Some(make_build_only_row(row)));
                    }

                    self.state = HashJoinState::Exhausted;
                    return Ok(None);
                }
                HashJoinState::Exhausted => return Ok(None),
            }
        }
    }

    async fn close(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.build_child.close(ctx).await?;
        self.probe_child.close(ctx).await?;
        self.state = HashJoinState::Exhausted;
        Ok(())
    }

    fn children(&self) -> Vec<&dyn PhysicalOperator> {
        if self.left_is_build {
            vec![self.build_child.as_ref(), self.probe_child.as_ref()]
        } else {
            vec![self.probe_child.as_ref(), self.build_child.as_ref()]
        }
    }

    fn children_mut(&mut self) -> Vec<&mut dyn PhysicalOperator> {
        if self.left_is_build {
            vec![self.build_child.as_mut(), self.probe_child.as_mut()]
        } else {
            vec![self.probe_child.as_mut(), self.build_child.as_mut()]
        }
    }

    fn name(&self) -> &'static str {
        "HashJoin"
    }

    fn explain_info(&self) -> Option<String> {
        let jt = match self.join_type {
            HashJoinType::Inner => "INNER",
            HashJoinType::Left => "LEFT",
            HashJoinType::Right => "RIGHT",
            HashJoinType::Full => "FULL",
        };
        Some(format!("type={}, left_is_build={}", jt, self.left_is_build))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ColumnDef, DataType};

    fn schema_left() -> TableSchema {
        TableSchema {
            name: "left".to_string(),
            table_id: 1,
            columns: vec![
                ColumnDef {
                    name: "id".to_string(),
                    data_type: DataType::Int32,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
                ColumnDef {
                    name: "l".to_string(),
                    data_type: DataType::Text,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        }
    }

    fn schema_right() -> TableSchema {
        TableSchema {
            name: "right".to_string(),
            table_id: 2,
            columns: vec![
                ColumnDef {
                    name: "id".to_string(),
                    data_type: DataType::Int32,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
                ColumnDef {
                    name: "r".to_string(),
                    data_type: DataType::Text,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        }
    }

    #[test]
    fn test_hash_int32_int64_compatible() {
        assert_eq!(
            hash_join_key(&[Value::Int32(42)]),
            hash_join_key(&[Value::Int64(42)])
        );
        assert!(join_keys_equal(&[Value::Int32(42)], &[Value::Int64(42)]));
    }

    #[test]
    fn test_numeric_normalization_hash_equal() {
        use rust_decimal::Decimal;
        use std::str::FromStr;

        let d1 = Decimal::from_str("1.0").unwrap();
        let d2 = Decimal::from_str("1.00").unwrap();
        assert_eq!(
            hash_join_key(&[Value::Numeric(d1)]),
            hash_join_key(&[Value::Numeric(d2)])
        );
        assert!(join_keys_equal(
            &[Value::Numeric(d1)],
            &[Value::Numeric(d2)]
        ));
    }

    #[test]
    fn test_float64_nan_hash_equal_and_join_equal() {
        // Use different NaN bit patterns to ensure the join treats all NaNs as equal.
        let nan1 = f64::from_bits(0x7ff8_0000_0000_0001);
        let nan2 = f64::from_bits(0xfff8_0000_0000_0002);
        assert!(nan1.is_nan());
        assert!(nan2.is_nan());

        assert!(join_keys_equal(
            &[Value::Float64(nan1)],
            &[Value::Float64(nan2)]
        ));
        assert_eq!(
            hash_join_key(&[Value::Float64(nan1)]),
            hash_join_key(&[Value::Float64(nan2)])
        );
    }

    #[test]
    fn test_float64_negative_zero_hash_equal_and_join_equal() {
        assert!(join_keys_equal(
            &[Value::Float64(-0.0)],
            &[Value::Float64(0.0)]
        ));
        assert_eq!(
            hash_join_key(&[Value::Float64(-0.0)]),
            hash_join_key(&[Value::Float64(0.0)])
        );
    }

    #[test]
    fn test_hash_table_insert_and_probe() {
        let mut table = JoinHashTable::new(vec![0]);
        table.insert(Row::new(vec![Value::Int32(1), Value::Text("a".into())]));
        table.insert(Row::new(vec![Value::Int32(1), Value::Text("b".into())]));
        table.insert(Row::new(vec![Value::Int32(2), Value::Text("c".into())]));
        table.finalize();

        // Probe by values using the table's key indices.
        let hash = hash_join_key(&[Value::Int32(1)]);
        let bucket = table.buckets.get(&hash).unwrap();
        assert_eq!(bucket.rows.len(), 2);

        assert!(table.row_key_equals_values(&bucket.rows[0], &[Value::Int32(1)]));
        assert!(table.row_key_equals_values(&bucket.rows[1], &[Value::Int32(1)]));
        assert!(!table.row_key_equals_values(&bucket.rows[0], &[Value::Int32(2)]));
    }

    #[test]
    fn test_hash_join_outer_side_mapping_is_logical() {
        // Validate the critical mapping: join type is relative to logical left/right,
        // while build/probe are chosen independently (left_is_build).
        let left_child: BoxedOperator =
            Box::new(super::super::scan::TableScanOperator::new(schema_left()));
        let right_child: BoxedOperator =
            Box::new(super::super::scan::TableScanOperator::new(schema_right()));

        let op = HashJoinOperator::new(
            left_child,
            right_child,
            HashJoinType::Left,
            vec![0],
            vec![0],
            true, // left is build
            None,
            HashJoinConfig::default(),
        );
        assert!(op.build_outer);
        assert!(!op.probe_outer);

        let left_child: BoxedOperator =
            Box::new(super::super::scan::TableScanOperator::new(schema_left()));
        let right_child: BoxedOperator =
            Box::new(super::super::scan::TableScanOperator::new(schema_right()));
        let op = HashJoinOperator::new(
            left_child,
            right_child,
            HashJoinType::Left,
            vec![0],
            vec![0],
            false, // right is build (left is probe)
            None,
            HashJoinConfig::default(),
        );
        assert!(!op.build_outer);
        assert!(op.probe_outer);
    }

    #[test]
    fn test_hash_table_empty_probe() {
        let table = JoinHashTable::new(vec![0]);

        let hash = hash_join_key(&[Value::Int32(1)]);
        assert!(table.buckets.get(&hash).is_none());
    }

    #[test]
    fn test_hash_table_many_duplicate_keys() {
        let mut table = JoinHashTable::new(vec![0]);
        for i in 0..100 {
            table.insert(Row::new(vec![
                Value::Int32(1),
                Value::Text(format!("row_{}", i)),
            ]));
        }
        table.finalize();

        let hash = hash_join_key(&[Value::Int32(1)]);
        let bucket = table.buckets.get(&hash).unwrap();
        assert_eq!(bucket.rows.len(), 100);

        for row in &bucket.rows {
            assert!(table.row_key_equals_values(row, &[Value::Int32(1)]));
        }
    }

    #[test]
    fn test_hash_join_full_outer_mapping() {
        let left_child: BoxedOperator =
            Box::new(super::super::scan::TableScanOperator::new(schema_left()));
        let right_child: BoxedOperator =
            Box::new(super::super::scan::TableScanOperator::new(schema_right()));

        let op = HashJoinOperator::new(
            left_child,
            right_child,
            HashJoinType::Full,
            vec![0],
            vec![0],
            true,
            None,
            HashJoinConfig::default(),
        );

        assert!(op.build_outer);
        assert!(op.probe_outer);
    }
}
