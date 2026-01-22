use anyhow::{anyhow, Result};
use async_trait::async_trait;
use sqlparser::ast::{Expr, JoinOperator};

use super::{collect_all, BoxedOperator, ExecutionContext, PhysicalOperator};
use crate::sql::expr::eval_expr;
use crate::types::{ColumnDef, Row, TableSchema, Value};

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

#[derive(Debug)]
pub struct NestedLoopJoinOperator {
    left: BoxedOperator,
    right: BoxedOperator,
    join_type: JoinType,
    condition: Option<Expr>,
    output_schema: TableSchema,
    result_rows: Vec<Row>,
    position: usize,
    opened: bool,
}

impl NestedLoopJoinOperator {
    pub fn new(
        left: BoxedOperator,
        right: BoxedOperator,
        join_type: JoinType,
        condition: Option<Expr>,
    ) -> Self {
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
        };

        Self {
            left,
            right,
            join_type,
            condition,
            output_schema,
            result_rows: Vec::new(),
            position: 0,
            opened: false,
        }
    }

    fn make_null_row(schema: &TableSchema) -> Row {
        Row::new(vec![Value::Null; schema.columns.len()])
    }

    fn concat_rows(left: &Row, right: &Row) -> Row {
        let mut values = left.values.clone();
        values.extend(right.values.clone());
        Row::new(values)
    }

    fn eval_condition(&self, combined_row: &Row) -> Result<bool> {
        if let Some(cond) = &self.condition {
            let result = eval_expr(cond, Some(combined_row), Some(&self.output_schema))?;
            match result {
                Value::Boolean(b) => Ok(b),
                Value::Null => Ok(false),
                _ => Err(anyhow!("Join condition must evaluate to boolean")),
            }
        } else {
            Ok(true)
        }
    }
}

#[async_trait]
impl PhysicalOperator for NestedLoopJoinOperator {
    fn schema(&self) -> &TableSchema {
        &self.output_schema
    }

    async fn open(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.left.open(ctx).await?;
        self.right.open(ctx).await?;

        let left_rows = collect_all(self.left.as_mut(), ctx).await?;
        let right_rows = collect_all(self.right.as_mut(), ctx).await?;

        let left_schema = self.left.schema();
        let right_schema = self.right.schema();

        self.result_rows.clear();

        match self.join_type {
            JoinType::Cross => {
                for left_row in &left_rows {
                    for right_row in &right_rows {
                        self.result_rows
                            .push(Self::concat_rows(left_row, right_row));
                    }
                }
            }
            JoinType::Inner => {
                for left_row in &left_rows {
                    for right_row in &right_rows {
                        let combined = Self::concat_rows(left_row, right_row);
                        if self.eval_condition(&combined)? {
                            self.result_rows.push(combined);
                        }
                    }
                }
            }
            JoinType::Left => {
                for left_row in &left_rows {
                    let mut matched = false;
                    for right_row in &right_rows {
                        let combined = Self::concat_rows(left_row, right_row);
                        if self.eval_condition(&combined)? {
                            self.result_rows.push(combined);
                            matched = true;
                        }
                    }
                    if !matched {
                        let null_right = Self::make_null_row(right_schema);
                        self.result_rows
                            .push(Self::concat_rows(left_row, &null_right));
                    }
                }
            }
            JoinType::Right => {
                for right_row in &right_rows {
                    let mut matched = false;
                    for left_row in &left_rows {
                        let combined = Self::concat_rows(left_row, right_row);
                        if self.eval_condition(&combined)? {
                            self.result_rows.push(combined);
                            matched = true;
                        }
                    }
                    if !matched {
                        let null_left = Self::make_null_row(left_schema);
                        self.result_rows
                            .push(Self::concat_rows(&null_left, right_row));
                    }
                }
            }
            JoinType::Full => {
                let mut right_matched = vec![false; right_rows.len()];

                for left_row in &left_rows {
                    let mut left_matched = false;
                    for (i, right_row) in right_rows.iter().enumerate() {
                        let combined = Self::concat_rows(left_row, right_row);
                        if self.eval_condition(&combined)? {
                            self.result_rows.push(combined);
                            left_matched = true;
                            right_matched[i] = true;
                        }
                    }
                    if !left_matched {
                        let null_right = Self::make_null_row(right_schema);
                        self.result_rows
                            .push(Self::concat_rows(left_row, &null_right));
                    }
                }

                for (i, right_row) in right_rows.iter().enumerate() {
                    if !right_matched[i] {
                        let null_left = Self::make_null_row(left_schema);
                        self.result_rows
                            .push(Self::concat_rows(&null_left, right_row));
                    }
                }
            }
        }

        self.position = 0;
        self.opened = true;
        Ok(())
    }

    async fn next(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>> {
        if !self.opened {
            return Err(anyhow!("Operator not opened"));
        }

        if self.position < self.result_rows.len() {
            let row = self.result_rows[self.position].clone();
            self.position += 1;
            Ok(Some(row))
        } else {
            Ok(None)
        }
    }

    async fn close(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.left.close(ctx).await?;
        self.right.close(ctx).await?;
        self.result_rows.clear();
        self.opened = false;
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
            Some(format!("type={}, on={}", join_type_str, cond))
        } else {
            Some(format!("type={}", join_type_str))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::operators::scan::TableScanOperator;
    use crate::types::DataType;
    use sqlparser::ast::{BinaryOperator, Ident};

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
                },
                ColumnDef {
                    name: "name".to_string(),
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
            pk_indices: vec![0],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
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
                },
                ColumnDef {
                    name: "user_id".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![0],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
        }
    }

    #[test]
    fn test_nested_loop_join_creation() {
        let left = Box::new(TableScanOperator::new(users_schema()));
        let right = Box::new(TableScanOperator::new(orders_schema()));

        let condition = Expr::BinaryOp {
            left: Box::new(Expr::Identifier(Ident::new("id"))),
            op: BinaryOperator::Eq,
            right: Box::new(Expr::Identifier(Ident::new("user_id"))),
        };

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
            JoinType::from(&JoinOperator::LeftOuter(sqlparser::ast::JoinConstraint::None)),
            JoinType::Left
        );
        assert_eq!(
            JoinType::from(&JoinOperator::RightOuter(
                sqlparser::ast::JoinConstraint::None
            )),
            JoinType::Right
        );
        assert_eq!(
            JoinType::from(&JoinOperator::FullOuter(sqlparser::ast::JoinConstraint::None)),
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
