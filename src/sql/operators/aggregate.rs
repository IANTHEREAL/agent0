use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use sqlparser::ast::{Expr, OrderByExpr};

use super::{collect_all, BoxedOperator, ExecutionContext, PhysicalOperator};
use crate::sql::expr::{compare_order_by_values, eval_expr};
use crate::sql::value_key::{serialize_value_for_key, serialize_values_for_key};
use crate::sql::Aggregator;
use crate::types::{ColumnDef, DataType, Row, TableSchema, Value};

#[derive(Debug, Clone)]
pub struct AggregateExpr {
    pub func_name: String,
    pub arg: Option<Expr>,
    pub distinct: bool,
    pub delimiter: Option<String>,
    pub filter: Option<Expr>,
    pub order_by: Vec<OrderByExpr>,
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
            from_alias: None,
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

    fn create_aggregator(agg_expr: &AggregateExpr) -> Result<Aggregator> {
        if agg_expr.func_name == "STRING_AGG" {
            let delim = agg_expr.delimiter.as_deref().unwrap_or(",").to_string();
            return Ok(Aggregator::new_string_agg(delim));
        }
        Aggregator::new(&agg_expr.func_name)
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
            ordered_agg_buffers: Vec<Option<Vec<(Vec<Value>, Value)>>>,
        }

        let mut groups: HashMap<Vec<u8>, GroupState> = HashMap::new();

        for row in &input_rows {
            let mut group_key_values = Vec::new();
            for expr in &self.group_by_exprs {
                let val = eval_expr(expr, Some(row), Some(input_schema))?;
                group_key_values.push(val);
            }

            let key_bytes = serialize_values_for_key(&group_key_values)
                .map_err(|e| anyhow!("Failed to serialize group key: {}", e))?;

            if !groups.contains_key(&key_bytes) {
                let aggregators: Vec<Aggregator> = self
                    .aggregate_exprs
                    .iter()
                    .map(|agg_expr| Self::create_aggregator(agg_expr))
                    .collect::<Result<Vec<_>>>()?;
                let seen_distinct = (0..self.aggregate_exprs.len())
                    .map(|_| HashSet::new())
                    .collect();
                let ordered_agg_buffers = self
                    .aggregate_exprs
                    .iter()
                    .map(|agg_expr| {
                        if agg_expr.func_name == "ARRAY_AGG" && !agg_expr.order_by.is_empty() {
                            Some(Vec::new())
                        } else {
                            None
                        }
                    })
                    .collect();
                groups.insert(
                    key_bytes.clone(),
                    GroupState {
                        group_values: group_key_values.clone(),
                        aggregators,
                        seen_distinct,
                        ordered_agg_buffers,
                    },
                );
            }

            let state = groups
                .get_mut(&key_bytes)
                .ok_or_else(|| anyhow!("Aggregate group state missing"))?;

            for (i, agg_expr) in self.aggregate_exprs.iter().enumerate() {
                if let Some(ref filter_expr) = agg_expr.filter {
                    let filter_val = eval_expr(filter_expr, Some(row), Some(input_schema))?;
                    if !matches!(filter_val, Value::Boolean(true)) {
                        continue;
                    }
                }

                let val = if let Some(arg) = &agg_expr.arg {
                    eval_expr(arg, Some(row), Some(input_schema))?
                } else {
                    Value::Int32(1)
                };

                if agg_expr.distinct {
                    let val_bytes = serialize_value_for_key(&val)
                        .map_err(|e| anyhow!("Failed to serialize DISTINCT value: {}", e))?;
                    if !state.seen_distinct[i].insert(val_bytes) {
                        continue;
                    }
                }

                if let Some(buf) = state.ordered_agg_buffers[i].as_mut() {
                    let mut keys = Vec::with_capacity(agg_expr.order_by.len());
                    for o in &agg_expr.order_by {
                        let key = eval_expr(&o.expr, Some(row), Some(input_schema))?;
                        keys.push(key);
                    }
                    buf.push((keys, val));
                } else {
                    state.aggregators[i].update(&val)?;
                }
            }
        }

        self.result_rows.clear();

        if groups.is_empty() && self.group_by_exprs.is_empty() {
            let mut values = Vec::new();
            for agg_expr in &self.aggregate_exprs {
                let agg = Self::create_aggregator(agg_expr)?;
                values.push(agg.result());
            }
            self.result_rows.push(Row::new(values));
        } else {
            for (_, state) in groups {
                let GroupState {
                    group_values,
                    aggregators,
                    mut ordered_agg_buffers,
                    ..
                } = state;
                let mut values = group_values;
                for (i, agg) in aggregators.into_iter().enumerate() {
                    if let Some(mut buf) = ordered_agg_buffers.get_mut(i).and_then(Option::take) {
                        let order_by = &self.aggregate_exprs[i].order_by;
                        buf.sort_by(|(keys_a, _), (keys_b, _)| {
                            for (key_idx, order_expr) in order_by.iter().enumerate() {
                                let asc = order_expr.asc.unwrap_or(true);
                                let nulls_first = order_expr.nulls_first.unwrap_or(!asc);
                                let ord = compare_order_by_values(
                                    &keys_a[key_idx],
                                    &keys_b[key_idx],
                                    asc,
                                    nulls_first,
                                );
                                if ord != std::cmp::Ordering::Equal {
                                    return ord;
                                }
                            }
                            std::cmp::Ordering::Equal
                        });
                        let sorted_values = buf.into_iter().map(|(_, v)| v).collect::<Vec<_>>();
                        values.push(if sorted_values.is_empty() {
                            Value::Null
                        } else {
                            Value::Array(sorted_values)
                        });
                    } else {
                        values.push(agg.result());
                    }
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
            from_alias: None,
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
            delimiter: None,
            filter: None,
            order_by: vec![],
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
                delimiter: None,
                filter: None,
                order_by: vec![],
            },
            AggregateExpr {
                func_name: "SUM".to_string(),
                arg: Some(Expr::Identifier(Ident::new("amount"))),
                distinct: false,
                delimiter: None,
                filter: None,
                order_by: vec![],
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
            delimiter: None,
            filter: None,
            order_by: vec![],
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

    #[test]
    fn test_hash_aggregate_multiple_group_columns() {
        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let group_by_exprs = vec![
            Expr::Identifier(Ident::new("category")),
            Expr::Identifier(Ident::new("amount")),
        ];
        let aggregate_exprs = vec![AggregateExpr {
            func_name: "COUNT".to_string(),
            arg: None,
            distinct: false,
            delimiter: None,
            filter: None,
            order_by: vec![],
        }];

        let op = HashAggregateOperator::new(
            child,
            group_by_exprs,
            aggregate_exprs,
            vec!["category".to_string(), "amount".to_string()],
            vec![DataType::Text, DataType::Int32],
            vec!["count".to_string()],
            vec![DataType::Int64],
        );

        assert_eq!(op.schema().columns.len(), 3);
        assert_eq!(op.schema().columns[0].name, "category");
        assert_eq!(op.schema().columns[1].name, "amount");
        assert_eq!(op.schema().columns[2].name, "count");

        let info = op.explain_info().unwrap();
        assert!(info.contains("category"));
        assert!(info.contains("amount"));
    }

    #[test]
    fn test_hash_aggregate_string_agg_delimiter() {
        let agg_expr = AggregateExpr {
            func_name: "STRING_AGG".to_string(),
            arg: Some(Expr::Identifier(Ident::new("category"))),
            distinct: false,
            delimiter: Some(", ".to_string()),
            filter: None,
            order_by: vec![],
        };

        let aggregator = HashAggregateOperator::create_aggregator(&agg_expr).unwrap();
        assert_eq!(aggregator.result(), Value::Null);
    }

    #[test]
    fn test_hash_aggregate_with_filter_creation() {
        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let aggregate_exprs = vec![AggregateExpr {
            func_name: "SUM".to_string(),
            arg: Some(Expr::Identifier(Ident::new("amount"))),
            distinct: false,
            delimiter: None,
            filter: Some(Expr::BinaryOp {
                left: Box::new(Expr::Identifier(Ident::new("amount"))),
                op: sqlparser::ast::BinaryOperator::Gt,
                right: Box::new(Expr::Value(sqlparser::ast::Value::Number(
                    "100".to_string(),
                    false,
                ))),
            }),
            order_by: vec![],
        }];

        let op = HashAggregateOperator::new(
            child,
            vec![],
            aggregate_exprs.clone(),
            vec![],
            vec![],
            vec!["filtered_sum".to_string()],
            vec![DataType::Int64],
        );

        assert_eq!(op.name(), "HashAggregate");
        assert!(op.aggregate_exprs[0].filter.is_some());
    }
}
