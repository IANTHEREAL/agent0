use anyhow::{anyhow, Result};
use async_trait::async_trait;
use sqlparser::ast::JoinOperator;

use super::{collect_all, BoxedOperator, ExecutionContext, PhysicalOperator};
use crate::model::{ColumnDef, Row, TableSchema, Value};
use crate::sql::analyzer::types::TypedExpr;
use crate::sql::expr::classify::needs_async;
use crate::sql::expr::typed_eval::eval_typed_expr;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum JoinType {
    Inner,
    Left,
    Right,
    Full,
    Cross,
}

impl From<&JoinOperator> for JoinType {
    fn from(op: &JoinOperator) -> Self {
        match op {
            JoinOperator::Inner(_) => JoinType::Inner,
            JoinOperator::LeftOuter(_) => JoinType::Left,
            JoinOperator::RightOuter(_) => JoinType::Right,
            JoinOperator::FullOuter(_) => JoinType::Full,
            JoinOperator::CrossJoin => JoinType::Cross,
            _ => JoinType::Inner,
        }
    }
}

/// Streaming nested-loop join operator.
///
/// Materializes only the right (inner) side during `open()`.  The left
/// (outer) side is streamed one row at a time through `next()`, which
/// keeps memory usage at O(|right|) instead of O(|left| × |right|).
#[derive(Debug)]
pub struct NestedLoopJoinOperator {
    left: BoxedOperator,
    right: BoxedOperator,
    join_type: JoinType,
    condition: Option<TypedExpr>,
    output_schema: TableSchema,

    // ── Streaming state ─────────────────────────────────────────────
    /// Materialized right (inner) side rows.
    right_rows: Vec<Row>,
    /// Column count for the left side (used for NULL padding).
    left_col_count: usize,
    /// Column count for the right side (used for NULL padding).
    right_col_count: usize,
    /// Current left row being probed against all right rows.
    current_left_row: Option<Row>,
    /// Position within `right_rows` for the current left row scan.
    right_pos: usize,
    /// Whether the current left row has matched any right row.
    left_had_match: bool,
    /// For RIGHT/FULL joins: tracks which right rows have been matched.
    right_matched: Vec<bool>,
    /// Current execution phase.
    phase: NLJPhase,
    /// Whether right side must be re-executed per left row (LATERAL/correlated).
    right_depends_on_outer: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum NLJPhase {
    NotOpened,
    /// Streaming left side, scanning right rows for each left row.
    Scanning,
    /// Emitting unmatched right rows (RIGHT/FULL join only).
    EmittingUnmatchedRight,
    Exhausted,
}

impl NestedLoopJoinOperator {
    #[allow(dead_code)] // framework: join path
    pub fn new(
        left: BoxedOperator,
        right: BoxedOperator,
        join_type: JoinType,
        condition: Option<TypedExpr>,
    ) -> Self {
        let left_col_count = left.schema().columns.len();
        let right_col_count = right.schema().columns.len();

        let mut columns = Vec::new();

        for col in &left.schema().columns {
            columns.push(ColumnDef {
                name: col.name.clone(),
                data_type: col.data_type.clone(),
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                generation_expr: None,
                generation_expr_authorized_by: None,
                collation: None,
                is_dropped: false,
            });
        }

        for col in &right.schema().columns {
            columns.push(ColumnDef {
                name: col.name.clone(),
                data_type: col.data_type.clone(),
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                generation_expr: None,
                generation_expr_authorized_by: None,
                collation: None,
                is_dropped: false,
            });
        }

        let output_schema = TableSchema {
            name: "join".to_string(),
            table_id: 0,
            columns,
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            rls_enabled: false,
            rls_force: false,
            from_alias: None,
        };

        Self {
            left,
            right,
            join_type,
            condition,
            output_schema,
            right_rows: Vec::new(),
            left_col_count,
            right_col_count,
            current_left_row: None,
            right_pos: 0,
            left_had_match: false,
            right_matched: Vec::new(),
            phase: NLJPhase::NotOpened,
            right_depends_on_outer: false,
        }
    }

    #[allow(dead_code)] // forward-compat: correlated/LATERAL subquery joins
    pub fn new_with_outer_dependency(
        left: BoxedOperator,
        right: BoxedOperator,
        join_type: JoinType,
        condition: Option<TypedExpr>,
        right_depends_on_outer: bool,
    ) -> Self {
        let mut op = Self::new(left, right, join_type, condition);
        op.right_depends_on_outer = right_depends_on_outer;
        op
    }

    #[allow(dead_code)] // forward-compat: correlated/LATERAL subquery joins
    pub fn with_outer_dependency(mut self, right_depends_on_outer: bool) -> Self {
        self.right_depends_on_outer = right_depends_on_outer;
        self
    }

    fn make_null_values(count: usize) -> Vec<Value> {
        vec![Value::Null; count]
    }

    fn concat_rows(left: &Row, right: &Row) -> Row {
        let mut values = left.values.clone();
        values.extend(right.values.clone());
        Row::new(values)
    }

    async fn eval_condition(
        &self,
        combined_row: &Row,
        ctx: &mut ExecutionContext<'_>,
    ) -> Result<bool> {
        if let Some(cond) = &self.condition {
            let result = if needs_async(cond) {
                let materialized = ctx
                    .executor
                    .materialize_expr_for_row(
                        cond,
                        combined_row,
                        ctx.outer_row.as_ref(),
                        Some(&self.output_schema),
                        ctx.txn,
                        ctx.db_id,
                        ctx.sequence_values,
                        ctx.search_path,
                        ctx.cte_tables,
                        ctx.query_ctx,
                    )
                    .await?;
                eval_typed_expr(&materialized, combined_row, ctx.query_ctx)?
            } else {
                eval_typed_expr(cond, combined_row, ctx.query_ctx)?
            };
            match result {
                Value::Boolean(b) => Ok(b),
                Value::Null => Ok(false),
                _ => Err(anyhow!("Join condition must evaluate to boolean")),
            }
        } else {
            Ok(true)
        }
    }

    async fn reload_right_rows_for_left(
        &mut self,
        left_row: &Row,
        ctx: &mut ExecutionContext<'_>,
    ) -> Result<()> {
        let saved_outer = ctx.outer_row.clone();
        ctx.outer_row = Some(left_row.clone());
        self.right.open(ctx).await?;
        self.right_rows = collect_all(self.right.as_mut(), ctx).await?;
        self.right.close(ctx).await?;
        ctx.outer_row = saved_outer;
        Ok(())
    }
}

#[async_trait]
impl PhysicalOperator for NestedLoopJoinOperator {
    fn schema(&self) -> &TableSchema {
        &self.output_schema
    }

    async fn open(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.left.open(ctx).await?;
        if self.right_depends_on_outer {
            if matches!(self.join_type, JoinType::Right | JoinType::Full) {
                return Err(anyhow!(
                    "RIGHT/FULL JOIN with correlated lateral subquery is not supported"
                ));
            }
            self.right_rows.clear();
            self.right_matched.clear();
        } else {
            self.right.open(ctx).await?;

            // Materialize only the right (inner) side.
            self.right_rows = collect_all(self.right.as_mut(), ctx).await?;

            // For RIGHT/FULL joins, track which right rows have been matched.
            if matches!(self.join_type, JoinType::Right | JoinType::Full) {
                self.right_matched = vec![false; self.right_rows.len()];
            }
        }

        self.current_left_row = None;
        self.right_pos = 0;
        self.left_had_match = false;
        self.phase = NLJPhase::Scanning;
        Ok(())
    }

    async fn next(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>> {
        loop {
            match self.phase {
                NLJPhase::NotOpened => return Err(anyhow!("Operator not opened")),

                NLJPhase::Scanning => {
                    // Ensure we have a current left row to probe.
                    if self.current_left_row.is_none() {
                        match self.left.next(ctx).await? {
                            Some(row) => {
                                if self.right_depends_on_outer {
                                    self.reload_right_rows_for_left(&row, ctx).await?;
                                }
                                self.current_left_row = Some(row);
                                self.right_pos = 0;
                                self.left_had_match = false;
                            }
                            None => {
                                // Left side exhausted.
                                if matches!(self.join_type, JoinType::Right | JoinType::Full) {
                                    self.phase = NLJPhase::EmittingUnmatchedRight;
                                    self.right_pos = 0;
                                    continue;
                                } else {
                                    self.phase = NLJPhase::Exhausted;
                                    return Ok(None);
                                }
                            }
                        }
                    }

                    let left_row = self.current_left_row.as_ref().unwrap();

                    // Scan remaining right rows for the current left row.
                    while self.right_pos < self.right_rows.len() {
                        let right_row = &self.right_rows[self.right_pos];
                        let combined = Self::concat_rows(left_row, right_row);
                        let idx = self.right_pos;
                        self.right_pos += 1;

                        if self.eval_condition(&combined, ctx).await? {
                            self.left_had_match = true;
                            if !self.right_matched.is_empty() {
                                self.right_matched[idx] = true;
                            }
                            return Ok(Some(combined));
                        }
                    }

                    // All right rows exhausted for this left row.
                    if matches!(self.join_type, JoinType::Left | JoinType::Full)
                        && !self.left_had_match
                    {
                        let null_right = Row::new(Self::make_null_values(self.right_col_count));
                        let result = Self::concat_rows(left_row, &null_right);
                        self.current_left_row = None;
                        return Ok(Some(result));
                    }

                    // Move to the next left row.
                    self.current_left_row = None;
                }

                NLJPhase::EmittingUnmatchedRight => {
                    while self.right_pos < self.right_rows.len() {
                        let idx = self.right_pos;
                        self.right_pos += 1;
                        if !self.right_matched[idx] {
                            let null_left = Row::new(Self::make_null_values(self.left_col_count));
                            let right_row = &self.right_rows[idx];
                            return Ok(Some(Self::concat_rows(&null_left, right_row)));
                        }
                    }
                    self.phase = NLJPhase::Exhausted;
                    return Ok(None);
                }

                NLJPhase::Exhausted => return Ok(None),
            }
        }
    }

    async fn close(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.left.close(ctx).await?;
        self.right.close(ctx).await?;
        self.right_rows.clear();
        self.right_matched.clear();
        self.current_left_row = None;
        self.phase = NLJPhase::Exhausted;
        Ok(())
    }

    fn children(&self) -> Vec<&dyn PhysicalOperator> {
        vec![self.left.as_ref(), self.right.as_ref()]
    }

    fn children_mut(&mut self) -> Vec<&mut dyn PhysicalOperator> {
        vec![self.left.as_mut(), self.right.as_mut()]
    }

    fn name(&self) -> &'static str {
        "NestedLoopJoin"
    }

    fn explain_info(&self) -> Option<String> {
        let join_type_str = match self.join_type {
            JoinType::Inner => "INNER",
            JoinType::Left => "LEFT",
            JoinType::Right => "RIGHT",
            JoinType::Full => "FULL",
            JoinType::Cross => "CROSS",
        };

        if let Some(cond) = &self.condition {
            Some(format!("type={}, on={:?}", join_type_str, cond))
        } else {
            Some(format!("type={}", join_type_str))
        }
    }
}

// Note: Behavioral tests (open/next/close correctness for INNER/LEFT/RIGHT/FULL)
// cannot be unit-tested here because `ExecutionContext` requires a live TiKV
// `Transaction`.  Row-producing join semantics are instead covered by SQL-level
// integration tests:
//   - tests/61_join_comprehensive.sql      (all 5 join types + NULL padding)
//   - tests/107_join_right_full_*.sql       (RIGHT/FULL outer join edge cases)
//   - tests/115_join_using_outer_*.sql      (SELECT * with outer joins)
//   - tests/116_join_using_outer_*.sql      (merged key COALESCE behavior)
//   - tests/149_inner-join.sql              (25 INNER JOIN scenarios)
// The unit tests below verify schema construction, type conversion, and EXPLAIN
// output for structural correctness of the operator.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DataType;
    use crate::sql::analyzer::types::{BinaryOp as TypedBinaryOp, TypedExprKind};
    use crate::sql::operators::scan::TableScanOperator;

    fn users_schema() -> TableSchema {
        TableSchema {
            name: "users".to_string(),
            table_id: 1,
            columns: vec![
                ColumnDef {
                    name: "id".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: true,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    generation_expr: None,
                    generation_expr_authorized_by: None,
                    collation: None,
                    is_dropped: false,
                },
                ColumnDef {
                    name: "name".to_string(),
                    data_type: DataType::Text,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    generation_expr: None,
                    generation_expr_authorized_by: None,
                    collation: None,
                    is_dropped: false,
                },
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![0],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            rls_enabled: false,
            rls_force: false,
            from_alias: None,
        }
    }

    fn orders_schema() -> TableSchema {
        TableSchema {
            name: "orders".to_string(),
            table_id: 2,
            columns: vec![
                ColumnDef {
                    name: "order_id".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: true,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    generation_expr: None,
                    generation_expr_authorized_by: None,
                    collation: None,
                    is_dropped: false,
                },
                ColumnDef {
                    name: "user_id".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    generation_expr: None,
                    generation_expr_authorized_by: None,
                    collation: None,
                    is_dropped: false,
                },
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![0],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            rls_enabled: false,
            rls_force: false,
            from_alias: None,
        }
    }

    /// Build the TypedExpr equivalent of `id = user_id` for the joined schema
    /// [id(0), name(1), order_id(2), user_id(3)].
    fn eq_condition_id_user_id() -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(TypedExpr {
                    kind: TypedExprKind::ColumnRef {
                        scope_depth: 0,
                        column_index: 0,
                        column_name: "id".to_string(),
                    },
                    data_type: DataType::Int32,
                }),
                op: TypedBinaryOp::Eq,
                right: Box::new(TypedExpr {
                    kind: TypedExprKind::ColumnRef {
                        scope_depth: 0,
                        column_index: 3,
                        column_name: "user_id".to_string(),
                    },
                    data_type: DataType::Int32,
                }),
            },
            data_type: DataType::Boolean,
        }
    }

    #[test]
    fn test_nested_loop_join_creation() {
        let left = Box::new(TableScanOperator::new(users_schema()));
        let right = Box::new(TableScanOperator::new(orders_schema()));

        let condition = eq_condition_id_user_id();

        let op = NestedLoopJoinOperator::new(left, right, JoinType::Inner, Some(condition));

        assert_eq!(op.name(), "NestedLoopJoin");
        assert_eq!(op.schema().columns.len(), 4);
        assert_eq!(op.schema().columns[0].name, "id");
        assert_eq!(op.schema().columns[1].name, "name");
        assert_eq!(op.schema().columns[2].name, "order_id");
        assert_eq!(op.schema().columns[3].name, "user_id");
    }

    #[test]
    fn test_nested_loop_join_explain_info() {
        let left = Box::new(TableScanOperator::new(users_schema()));
        let right = Box::new(TableScanOperator::new(orders_schema()));

        let op = NestedLoopJoinOperator::new(left, right, JoinType::Left, None);

        let info = op.explain_info().unwrap();
        assert!(info.contains("LEFT"));
    }

    #[test]
    fn test_join_type_from_operator() {
        assert_eq!(
            JoinType::from(&JoinOperator::Inner(sqlparser::ast::JoinConstraint::None)),
            JoinType::Inner
        );
        assert_eq!(
            JoinType::from(&JoinOperator::LeftOuter(
                sqlparser::ast::JoinConstraint::None
            )),
            JoinType::Left
        );
        assert_eq!(
            JoinType::from(&JoinOperator::RightOuter(
                sqlparser::ast::JoinConstraint::None
            )),
            JoinType::Right
        );
        assert_eq!(
            JoinType::from(&JoinOperator::FullOuter(
                sqlparser::ast::JoinConstraint::None
            )),
            JoinType::Full
        );
        assert_eq!(JoinType::from(&JoinOperator::CrossJoin), JoinType::Cross);
    }

    #[test]
    fn test_cross_join_schema() {
        let left = Box::new(TableScanOperator::new(users_schema()));
        let right = Box::new(TableScanOperator::new(orders_schema()));

        let op = NestedLoopJoinOperator::new(left, right, JoinType::Cross, None);

        assert_eq!(op.schema().columns.len(), 4);
        let info = op.explain_info().unwrap();
        assert!(info.contains("CROSS"));
    }
}
