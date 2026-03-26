//! Hash-based semi/anti join operator.
//!
//! Implements EXISTS / NOT EXISTS decorrelation results:
//! - **Semi-join**: emit left row on FIRST match in build side (right), skip to next.
//! - **Anti-join**: emit left row only if ZERO matches in build side.
//!
//! Build side is always the right child. Output schema = left-side columns ONLY.
//! Reuses `JoinHashTable` from `hash_join/hash_table.rs`.

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use super::hash_join::{
    hash_row_key_for_join, row_key_has_null_for_join, row_keys_equal_for_join, JoinHashTable,
};
use super::{BoxedOperator, ExecutionContext, PhysicalOperator};
use crate::model::{ColumnDef, Row, TableSchema};
use crate::pool::try_grow_statement_memory_scope;

#[derive(Debug)]
enum SemiJoinState {
    Created,
    Probing { hash_table: JoinHashTable },
    Exhausted,
}

/// Hash-based semi/anti join operator (Volcano iterator model).
///
/// - Semi (`anti = false`): emit left row if ≥1 match in build side.
/// - Anti (`anti = true`): emit left row if 0 matches in build side.
///
/// NULL key handling: NULL never matches (same semantics as HashJoinOperator).
#[derive(Debug)]
pub struct HashSemiJoinOperator {
    left_child: BoxedOperator,
    right_child: BoxedOperator,
    anti: bool,
    left_key_indices: Vec<usize>,
    right_key_indices: Vec<usize>,
    output_schema: TableSchema,
    state: SemiJoinState,
}

impl HashSemiJoinOperator {
    pub fn new(
        left_child: BoxedOperator,
        right_child: BoxedOperator,
        anti: bool,
        left_key_indices: Vec<usize>,
        right_key_indices: Vec<usize>,
    ) -> Self {
        // Output schema = left-side columns ONLY
        let columns: Vec<ColumnDef> = left_child
            .schema()
            .columns
            .iter()
            .map(|col| ColumnDef::new(col.name.clone(), col.data_type.clone(), col.nullable))
            .collect();

        let output_schema = TableSchema::virtual_table("hash_semi_join", columns);

        Self {
            left_child,
            right_child,
            anti,
            left_key_indices,
            right_key_indices,
            output_schema,
            state: SemiJoinState::Created,
        }
    }
}

#[async_trait]
impl PhysicalOperator for HashSemiJoinOperator {
    fn schema(&self) -> &TableSchema {
        &self.output_schema
    }

    async fn open(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.left_child.open(ctx).await?;
        self.right_child.open(ctx).await?;

        // Build phase: materialize right child into hash table
        let mut hash_table = JoinHashTable::with_capacity(
            self.right_key_indices.clone(),
            self.right_child.estimated_rows().unwrap_or(0),
        );
        let max_memory = crate::session_context::current_hash_join_work_mem();
        while let Some(row) = self.right_child.next(ctx).await? {
            let delta = hash_table.insert(row);
            try_grow_statement_memory_scope("operators.hash_semi_join.build", delta)?;
            if max_memory > 0 && hash_table.memory_bytes() > max_memory {
                return Err(anyhow::anyhow!(
                    "hash join build side exceeded memory limit: \
                     used {} bytes, limit {} bytes (db9.hash_join_work_mem). \
                     Reduce build-side cardinality with WHERE/LIMIT or \
                     increase db9.hash_join_work_mem",
                    hash_table.memory_bytes(),
                    max_memory,
                ));
            }
        }
        hash_table.finalize();

        self.state = SemiJoinState::Probing { hash_table };
        Ok(())
    }

    async fn next(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>> {
        let anti = self.anti;
        let left_key_indices = self.left_key_indices.as_slice();
        let right_key_indices = self.right_key_indices.as_slice();

        loop {
            match &self.state {
                SemiJoinState::Created => return Err(anyhow!("HashSemiJoinOperator not opened")),
                SemiJoinState::Exhausted => return Ok(None),
                SemiJoinState::Probing { .. } => {}
            }

            let hash_table = match &self.state {
                SemiJoinState::Probing { hash_table } => hash_table,
                _ => unreachable!(),
            };

            // Fetch next left (probe) row
            let left_row = match self.left_child.next(ctx).await? {
                Some(row) => row,
                None => {
                    self.state = SemiJoinState::Exhausted;
                    return Ok(None);
                }
            };

            // Check for NULL keys — NULL never matches
            let has_null = row_key_has_null_for_join(&left_row, left_key_indices);

            let has_match = if has_null {
                false
            } else {
                let hash = hash_row_key_for_join(&left_row, left_key_indices);
                if let Some((bucket_rows, _indices)) = hash_table.bucket_by_hash(hash) {
                    bucket_rows.iter().any(|build_row| {
                        row_keys_equal_for_join(
                            build_row,
                            right_key_indices,
                            &left_row,
                            left_key_indices,
                        )
                    })
                } else {
                    false
                }
            };

            // Semi: emit on match. Anti: emit on no-match.
            if anti {
                if !has_match {
                    return Ok(Some(left_row));
                }
            } else if has_match {
                return Ok(Some(left_row));
            }
            // No emit — try next left row
        }
    }

    async fn close(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.left_child.close(ctx).await?;
        self.right_child.close(ctx).await?;
        self.state = SemiJoinState::Exhausted;
        Ok(())
    }

    #[cfg(test)]
    fn children(&self) -> Vec<&dyn PhysicalOperator> {
        vec![self.left_child.as_ref(), self.right_child.as_ref()]
    }

    #[cfg(test)]
    fn children_mut(&mut self) -> Vec<&mut dyn PhysicalOperator> {
        vec![self.left_child.as_mut(), self.right_child.as_mut()]
    }

    #[cfg(test)]
    fn name(&self) -> &'static str {
        if self.anti {
            "HashAntiJoin"
        } else {
            "HashSemiJoin"
        }
    }

    #[cfg(test)]
    fn explain_info(&self) -> Option<String> {
        Some(format!("anti={}", self.anti))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DataType, Value};
    use async_trait::async_trait;

    #[derive(Debug)]
    struct TestOp {
        schema: TableSchema,
    }

    #[async_trait]
    impl PhysicalOperator for TestOp {
        fn schema(&self) -> &TableSchema {
            &self.schema
        }
        async fn open(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<()> {
            Ok(())
        }
        async fn next(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>> {
            let _ = Value::Null;
            Ok(None)
        }
        async fn close(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<()> {
            Ok(())
        }
    }

    fn make_schema(name: &str, cols: &[(&str, DataType, bool)]) -> TableSchema {
        TableSchema::new(
            name.to_string(),
            1,
            cols.iter()
                .map(|(n, dt, nullable)| ColumnDef::new(*n, dt.clone(), *nullable))
                .collect(),
            vec![],
        )
    }

    #[test]
    fn constructor_uses_left_schema_as_output_schema() {
        let left_schema = make_schema(
            "left",
            &[
                ("id", DataType::Int64, false),
                ("name", DataType::Text, true),
            ],
        );
        let right_schema = make_schema("right", &[("id", DataType::Int64, false)]);

        let op = HashSemiJoinOperator::new(
            Box::new(TestOp {
                schema: left_schema,
            }),
            Box::new(TestOp {
                schema: right_schema,
            }),
            false,
            vec![0],
            vec![0],
        );

        assert_eq!(op.schema().name, "hash_semi_join");
        assert_eq!(op.schema().columns.len(), 2);
        assert_eq!(op.schema().columns[0].name, "id");
        assert_eq!(op.schema().columns[1].name, "name");
    }

    #[test]
    fn name_and_explain_info_follow_anti_flag() {
        let left_schema = make_schema("left", &[("id", DataType::Int64, false)]);
        let right_schema = make_schema("right", &[("id", DataType::Int64, false)]);

        let semi = HashSemiJoinOperator::new(
            Box::new(TestOp {
                schema: left_schema.clone(),
            }),
            Box::new(TestOp {
                schema: right_schema.clone(),
            }),
            false,
            vec![0],
            vec![0],
        );
        assert_eq!(semi.name(), "HashSemiJoin");
        assert_eq!(semi.explain_info().as_deref(), Some("anti=false"));

        let anti = HashSemiJoinOperator::new(
            Box::new(TestOp {
                schema: left_schema,
            }),
            Box::new(TestOp {
                schema: right_schema,
            }),
            true,
            vec![0],
            vec![0],
        );
        assert_eq!(anti.name(), "HashAntiJoin");
        assert_eq!(anti.explain_info().as_deref(), Some("anti=true"));
    }

    #[test]
    fn children_and_children_mut_expose_both_inputs() {
        let left_schema = make_schema("left", &[("id", DataType::Int64, false)]);
        let right_schema = make_schema("right", &[("id", DataType::Int64, false)]);

        let mut op = HashSemiJoinOperator::new(
            Box::new(TestOp {
                schema: left_schema,
            }),
            Box::new(TestOp {
                schema: right_schema,
            }),
            false,
            vec![0],
            vec![0],
        );

        assert_eq!(op.children().len(), 2);
        assert_eq!(op.children_mut().len(), 2);
    }
}
