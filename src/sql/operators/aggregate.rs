use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use sqlparser::ast::Expr;

use super::{collect_all, BoxedOperator, ExecutionContext, PhysicalOperator};
use crate::sql::expr::eval_expr;
use crate::sql::Aggregator;
use crate::types::{ColumnDef, DataType, Row, TableSchema, Value};

#[derive(Debug, Clone)]
pub struct AggregateExpr {
    pub func_name: String,
    pub arg: Option<Expr>,
    pub distinct: bool,
}

#[derive(Debug)]
pub struct HashAggregateOperator {
    child: BoxedOperator,
    group_by_exprs: Vec<Expr>,
    aggregate_exprs: Vec<AggregateExpr>,
    output_schema: TableSchema,
    result_rows: Vec<Row>,
    position: usize,
    opened: bool,
}

impl HashAggregateOperator {
    pub fn new(
        child: BoxedOperator,
        group_by_exprs: Vec<Expr>,
        aggregate_exprs: Vec<AggregateExpr>,
        group_by_names: Vec<String>,
        group_by_types: Vec<DataType>,
        aggregate_names: Vec<String>,
        aggregate_types: Vec<DataType>,
    ) -> Self {
        let mut columns = Vec::new();

        for (name, dt) in group_by_names.iter().zip(group_by_types.iter()) {
            columns.push(ColumnDef {
                name: name.clone(),
                data_type: dt.clone(),
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            });
        }

        for (name, dt) in aggregate_names.iter().zip(aggregate_types.iter()) {
            columns.push(ColumnDef {
                name: name.clone(),
                data_type: dt.clone(),
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            });
        }

        let output_schema = TableSchema {
            name: "aggregate".to_string(),
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
            child,
            group_by_exprs,
            aggregate_exprs,
            output_schema,
            result_rows: Vec::new(),
            position: 0,
            opened: false,
        }
    }

    fn create_aggregator(func_name: &str) -> Result<Aggregator> {
        Aggregator::new(func_name)
    }
}

#[async_trait]
impl PhysicalOperator for HashAggregateOperator {
    fn schema(&self) -> &TableSchema {
        &self.output_schema
    }

    async fn open(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.child.open(ctx).await?;

        let input_rows = collect_all(self.child.as_mut(), ctx).await?;
        let input_schema = self.child.schema();

        struct GroupState {
            group_values: Vec<Value>,
            aggregators: Vec<Aggregator>,
            seen_distinct: Vec<HashSet<Vec<u8>>>,
        }

        let mut groups: HashMap<Vec<u8>, GroupState> = HashMap::new();

        for row in &input_rows {
            let mut group_key_values = Vec::new();
            for expr in &self.group_by_exprs {
                let val = eval_expr(expr, Some(row), Some(input_schema))?;
                group_key_values.push(val);
            }

            let key_bytes = bincode::serialize(&group_key_values)
                .map_err(|e| anyhow!("Failed to serialize group key: {}", e))?;

            if !groups.contains_key(&key_bytes) {
                let aggregators: Vec<Aggregator> = self
                    .aggregate_exprs
                    .iter()
                    .map(|agg_expr| Self::create_aggregator(&agg_expr.func_name))
                    .collect::<Result<Vec<_>>>()?;
                let seen_distinct = (0..self.aggregate_exprs.len())
                    .map(|_| HashSet::new())
                    .collect();
                groups.insert(
                    key_bytes.clone(),
                    GroupState {
                        group_values: group_key_values.clone(),
                        aggregators,
                        seen_distinct,
                    },
                );
            }

            let state = groups
                .get_mut(&key_bytes)
                .ok_or_else(|| anyhow!("Aggregate group state missing"))?;

            for (i, agg_expr) in self.aggregate_exprs.iter().enumerate() {
                let val = if let Some(arg) = &agg_expr.arg {
                    eval_expr(arg, Some(row), Some(input_schema))?
                } else {
                    Value::Int32(1)
                };

                if agg_expr.distinct {
                    let val_bytes = bincode::serialize(&val)
                        .map_err(|e| anyhow!("Failed to serialize DISTINCT value: {}", e))?;
                    if !state.seen_distinct[i].insert(val_bytes) {
                        continue;
                    }
                }

                state.aggregators[i].update(&val)?;
            }
        }

        self.result_rows.clear();

        if groups.is_empty() && self.group_by_exprs.is_empty() {
            let mut values = Vec::new();
            for agg_expr in &self.aggregate_exprs {
                let agg = Self::create_aggregator(&agg_expr.func_name)?;
                values.push(agg.result());
            }
            self.result_rows.push(Row::new(values));
        } else {
            for (_, state) in groups {
                let mut values = state.group_values;
                for agg in state.aggregators {
                    values.push(agg.result());
                }
                self.result_rows.push(Row::new(values));
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
        self.child.close(ctx).await?;
        self.result_rows.clear();
        self.opened = false;
        Ok(())
    }

    fn children(&self) -> Vec<&dyn PhysicalOperator> {
        vec![self.child.as_ref()]
    }

    fn children_mut(&mut self) -> Vec<&mut dyn PhysicalOperator> {
        vec![self.child.as_mut()]
    }

    fn name(&self) -> &'static str {
        "HashAggregate"
    }

    fn explain_info(&self) -> Option<String> {
        let group_cols: Vec<String> = self
            .group_by_exprs
            .iter()
            .map(|e| format!("{}", e))
            .collect();
        let agg_funcs: Vec<String> = self
            .aggregate_exprs
            .iter()
            .map(|a| a.func_name.clone())
            .collect();

        if group_cols.is_empty() {
            Some(format!("aggs=[{}]", agg_funcs.join(", ")))
        } else {
            Some(format!(
                "group_by=[{}], aggs=[{}]",
                group_cols.join(", "),
                agg_funcs.join(", ")
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::operators::scan::TableScanOperator;
    use sqlparser::ast::Ident;

    fn test_schema() -> TableSchema {
        TableSchema {
            name: "sales".to_string(),
            table_id: 1,
            columns: vec![
                ColumnDef {
                    name: "category".to_string(),
                    data_type: DataType::Text,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
                ColumnDef {
                    name: "amount".to_string(),
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
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
        }
    }

    #[test]
    fn test_hash_aggregate_creation() {
        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let group_by_exprs = vec![Expr::Identifier(Ident::new("category"))];
        let aggregate_exprs = vec![AggregateExpr {
            func_name: "SUM".to_string(),
            arg: Some(Expr::Identifier(Ident::new("amount"))),
            distinct: false,
        }];

        let op = HashAggregateOperator::new(
            child,
            group_by_exprs,
            aggregate_exprs,
            vec!["category".to_string()],
            vec![DataType::Text],
            vec!["sum_amount".to_string()],
            vec![DataType::Int64],
        );

        assert_eq!(op.name(), "HashAggregate");
        assert_eq!(op.schema().columns.len(), 2);
        assert_eq!(op.schema().columns[0].name, "category");
        assert_eq!(op.schema().columns[1].name, "sum_amount");
    }

    #[test]
    fn test_hash_aggregate_explain_info() {
        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let group_by_exprs = vec![Expr::Identifier(Ident::new("category"))];
        let aggregate_exprs = vec![
            AggregateExpr {
                func_name: "COUNT".to_string(),
                arg: None,
                distinct: false,
            },
            AggregateExpr {
                func_name: "SUM".to_string(),
                arg: Some(Expr::Identifier(Ident::new("amount"))),
                distinct: false,
            },
        ];

        let op = HashAggregateOperator::new(
            child,
            group_by_exprs,
            aggregate_exprs,
            vec!["category".to_string()],
            vec![DataType::Text],
            vec!["count".to_string(), "sum_amount".to_string()],
            vec![DataType::Int64, DataType::Int64],
        );

        let info = op.explain_info().unwrap();
        assert!(info.contains("group_by="));
        assert!(info.contains("category"));
        assert!(info.contains("COUNT"));
        assert!(info.contains("SUM"));
    }

    #[test]
    fn test_hash_aggregate_no_group_by() {
        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let aggregate_exprs = vec![AggregateExpr {
            func_name: "COUNT".to_string(),
            arg: None,
            distinct: false,
        }];

        let op = HashAggregateOperator::new(
            child,
            vec![],
            aggregate_exprs,
            vec![],
            vec![],
            vec!["count".to_string()],
            vec![DataType::Int64],
        );

        let info = op.explain_info().unwrap();
        assert!(info.contains("aggs="));
        assert!(!info.contains("group_by="));
    }
}
