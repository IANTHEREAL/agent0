use std::collections::HashSet;

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use super::{collect_all, BoxedOperator, ExecutionContext, PhysicalOperator};
use crate::sql::value_key::serialize_values_for_key;
use crate::types::{Row, TableSchema};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SetOperationType {
    Union,
    UnionAll,
    Intersect,
    IntersectAll,
    Except,
    ExceptAll,
}

#[derive(Debug)]
pub struct SetOperationOperator {
    left: BoxedOperator,
    right: BoxedOperator,
    op_type: SetOperationType,
    result_rows: Vec<Row>,
    position: usize,
    opened: bool,
}

impl SetOperationOperator {
    pub fn new(left: BoxedOperator, right: BoxedOperator, op_type: SetOperationType) -> Self {
        Self {
            left,
            right,
            op_type,
            result_rows: Vec::new(),
            position: 0,
            opened: false,
        }
    }

    fn row_to_key(row: &Row) -> Vec<u8> {
        serialize_values_for_key(&row.values).unwrap_or_default()
    }
}

#[async_trait]
impl PhysicalOperator for SetOperationOperator {
    fn schema(&self) -> &TableSchema {
        self.left.schema()
    }

    async fn open(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.left.open(ctx).await?;
        self.right.open(ctx).await?;

        let left_rows = collect_all(self.left.as_mut(), ctx).await?;
        let right_rows = collect_all(self.right.as_mut(), ctx).await?;

        self.result_rows.clear();

        match self.op_type {
            SetOperationType::UnionAll => {
                self.result_rows.extend(left_rows);
                self.result_rows.extend(right_rows);
            }
            SetOperationType::Union => {
                let mut seen: HashSet<Vec<u8>> = HashSet::new();
                for row in left_rows.into_iter().chain(right_rows.into_iter()) {
                    let key = Self::row_to_key(&row);
                    if seen.insert(key) {
                        self.result_rows.push(row);
                    }
                }
            }
            SetOperationType::Intersect => {
                let right_keys: HashSet<Vec<u8>> =
                    right_rows.iter().map(Self::row_to_key).collect();
                let mut seen: HashSet<Vec<u8>> = HashSet::new();
                for row in left_rows {
                    let key = Self::row_to_key(&row);
                    if right_keys.contains(&key) && seen.insert(key) {
                        self.result_rows.push(row);
                    }
                }
            }
            SetOperationType::IntersectAll => {
                let mut right_counts: std::collections::HashMap<Vec<u8>, usize> =
                    std::collections::HashMap::new();
                for row in &right_rows {
                    let key = Self::row_to_key(row);
                    *right_counts.entry(key).or_insert(0) += 1;
                }
                for row in left_rows {
                    let key = Self::row_to_key(&row);
                    if let Some(count) = right_counts.get_mut(&key) {
                        if *count > 0 {
                            *count -= 1;
                            self.result_rows.push(row);
                        }
                    }
                }
            }
            SetOperationType::Except => {
                let right_keys: HashSet<Vec<u8>> =
                    right_rows.iter().map(Self::row_to_key).collect();
                let mut seen: HashSet<Vec<u8>> = HashSet::new();
                for row in left_rows {
                    let key = Self::row_to_key(&row);
                    if !right_keys.contains(&key) && seen.insert(key) {
                        self.result_rows.push(row);
                    }
                }
            }
            SetOperationType::ExceptAll => {
                let mut right_counts: std::collections::HashMap<Vec<u8>, usize> =
                    std::collections::HashMap::new();
                for row in &right_rows {
                    let key = Self::row_to_key(row);
                    *right_counts.entry(key).or_insert(0) += 1;
                }
                for row in left_rows {
                    let key = Self::row_to_key(&row);
                    if let Some(count) = right_counts.get_mut(&key) {
                        if *count > 0 {
                            *count -= 1;
                            continue;
                        }
                    }
                    self.result_rows.push(row);
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
        match self.op_type {
            SetOperationType::Union => "Union",
            SetOperationType::UnionAll => "UnionAll",
            SetOperationType::Intersect => "Intersect",
            SetOperationType::IntersectAll => "IntersectAll",
            SetOperationType::Except => "Except",
            SetOperationType::ExceptAll => "ExceptAll",
        }
    }

    fn explain_info(&self) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::operators::scan::TableScanOperator;
    use crate::types::{ColumnDef, DataType, Value};

    fn test_schema() -> TableSchema {
        TableSchema {
            name: "test".to_string(),
            table_id: 1,
            columns: vec![ColumnDef {
                name: "id".to_string(),
                data_type: DataType::Int32,
                nullable: false,
                primary_key: true,
                unique: false,
                is_serial: false,
                default_expr: None,
            }],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![0],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        }
    }

    #[test]
    fn test_set_operation_union_creation() {
        let schema = test_schema();
        let left = Box::new(TableScanOperator::new(schema.clone()));
        let right = Box::new(TableScanOperator::new(schema));

        let op = SetOperationOperator::new(left, right, SetOperationType::Union);

        assert_eq!(op.name(), "Union");
    }

    #[test]
    fn test_set_operation_intersect_creation() {
        let schema = test_schema();
        let left = Box::new(TableScanOperator::new(schema.clone()));
        let right = Box::new(TableScanOperator::new(schema));

        let op = SetOperationOperator::new(left, right, SetOperationType::Intersect);

        assert_eq!(op.name(), "Intersect");
    }

    #[test]
    fn test_set_operation_except_creation() {
        let schema = test_schema();
        let left = Box::new(TableScanOperator::new(schema.clone()));
        let right = Box::new(TableScanOperator::new(schema));

        let op = SetOperationOperator::new(left, right, SetOperationType::Except);

        assert_eq!(op.name(), "Except");
    }

    #[test]
    fn test_row_to_key_canonicalizes_negative_zero() {
        let row_neg = Row::new(vec![Value::Float64(-0.0)]);
        let row_pos = Row::new(vec![Value::Float64(0.0)]);
        assert_eq!(
            SetOperationOperator::row_to_key(&row_neg),
            SetOperationOperator::row_to_key(&row_pos)
        );
    }

    #[test]
    fn test_row_to_key_canonicalizes_nan_payloads() {
        let nan1 = f64::from_bits(0x7ff8_0000_0000_0001);
        let nan2 = f64::from_bits(0x7ff8_0000_0000_0002);
        assert!(nan1.is_nan() && nan2.is_nan());

        let row1 = Row::new(vec![Value::Float64(nan1)]);
        let row2 = Row::new(vec![Value::Float64(nan2)]);
        assert_eq!(
            SetOperationOperator::row_to_key(&row1),
            SetOperationOperator::row_to_key(&row2)
        );
    }

    #[test]
    fn test_row_to_key_canonicalizes_numeric_scales() {
        use rust_decimal::Decimal;
        use std::str::FromStr;

        let d1 = Decimal::from_str("1.0").unwrap();
        let d2 = Decimal::from_str("1.00").unwrap();

        let row1 = Row::new(vec![Value::Numeric(d1)]);
        let row2 = Row::new(vec![Value::Numeric(d2)]);
        assert_eq!(
            SetOperationOperator::row_to_key(&row1),
            SetOperationOperator::row_to_key(&row2)
        );
    }

    #[test]
    fn test_row_to_key_multi_column_distinct() {
        let row_a = Row::new(vec![Value::Int32(1), Value::Text("a".to_string())]);
        let row_b = Row::new(vec![Value::Int32(1), Value::Text("b".to_string())]);
        let row_a2 = Row::new(vec![Value::Int32(1), Value::Text("a".to_string())]);

        assert_ne!(
            SetOperationOperator::row_to_key(&row_a),
            SetOperationOperator::row_to_key(&row_b)
        );
        assert_eq!(
            SetOperationOperator::row_to_key(&row_a),
            SetOperationOperator::row_to_key(&row_a2)
        );
    }

    #[test]
    fn test_row_to_key_null_values() {
        let row_null = Row::new(vec![Value::Null]);
        let row_int = Row::new(vec![Value::Int32(0)]);

        assert_ne!(
            SetOperationOperator::row_to_key(&row_null),
            SetOperationOperator::row_to_key(&row_int)
        );

        let row_null2 = Row::new(vec![Value::Null]);
        assert_eq!(
            SetOperationOperator::row_to_key(&row_null),
            SetOperationOperator::row_to_key(&row_null2)
        );
    }

    #[test]
    fn test_set_operation_all_variant_names() {
        let schema = test_schema();

        let variants = vec![
            (SetOperationType::Union, "Union"),
            (SetOperationType::UnionAll, "UnionAll"),
            (SetOperationType::Intersect, "Intersect"),
            (SetOperationType::IntersectAll, "IntersectAll"),
            (SetOperationType::Except, "Except"),
            (SetOperationType::ExceptAll, "ExceptAll"),
        ];

        for (op_type, expected_name) in variants {
            let left = Box::new(TableScanOperator::new(schema.clone()));
            let right = Box::new(TableScanOperator::new(schema.clone()));
            let op = SetOperationOperator::new(left, right, op_type);
            assert_eq!(op.name(), expected_name);
        }
    }
}
