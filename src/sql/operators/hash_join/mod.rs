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

mod hash_table;

#[cfg(test)]
mod tests;

// Re-export public API
#[allow(unused_imports)]
pub(crate) use hash_table::{
    hash_join_key, hash_row_key_for_join, join_keys_equal, row_key_has_null_for_join,
    row_keys_equal_for_join, JoinHashTable,
};

use hash_table::HashBucket;

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use super::{BoxedOperator, ExecutionContext, PhysicalOperator};
use crate::sql::analyzer::types::TypedExpr;
use crate::sql::expr::classify::needs_async;
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::types::{ColumnDef, Row, TableSchema, Value};

async fn eval_join_filter(
    filter: Option<&TypedExpr>,
    row: &Row,
    output_schema: &TableSchema,
    ctx: &mut ExecutionContext<'_>,
) -> Result<bool> {
    match filter {
        None => Ok(true),
        Some(expr) => {
            let result = if needs_async(expr) {
                let materialized = ctx
                    .executor
                    .materialize_expr_for_row(
                        expr,
                        row,
                        ctx.outer_row.as_ref(),
                        Some(output_schema),
                        ctx.txn,
                        ctx.db_id,
                        ctx.sequence_values,
                        ctx.search_path,
                        ctx.cte_tables,
                        ctx.query_ctx,
                    )
                    .await?;
                eval_typed_expr(&materialized, row, ctx.query_ctx)?
            } else {
                eval_typed_expr(expr, row, ctx.query_ctx)?
            };
            match result {
                Value::Boolean(b) => Ok(b),
                Value::Null => Ok(false),
                _ => Err(anyhow!("JOIN filter must be boolean")),
            }
        }
    }
}

/// Configuration for hash join planning/execution.
#[derive(Debug, Clone)]
pub struct HashJoinConfig {
    /// Maximum memory for the build-side hash table (bytes).
    pub max_memory_bytes: usize,
}

impl Default for HashJoinConfig {
    fn default() -> Self {
        Self {
            max_memory_bytes: 256 * 1024 * 1024,
        }
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
    #[allow(dead_code)] // framework: hash join operator
    join_type: HashJoinType,
    left_is_build: bool,
    build_key_indices: Vec<usize>,
    probe_key_indices: Vec<usize>,
    build_outer: bool,
    probe_outer: bool,
    filter: Option<TypedExpr>,
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
        filter: Option<TypedExpr>,
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
                collation: None,
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
                collation: None,
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
}

#[allow(dead_code)] // framework: hash join operator
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
                                    if !eval_join_filter(filter, &out, output_schema, ctx).await? {
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
