use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use super::key_encoding::{encode_value_key, encode_values_key};
use super::{collect_all, BoxedOperator, ExecutionContext, PhysicalOperator};
use crate::model::{ColumnDef, DataType, Row, TableSchema, Value};
use crate::pool::{try_grow_statement_memory_scope, try_shrink_statement_memory_scope};
use crate::sql::analyzer::types::{TypedExpr, TypedOrderByExpr};
use crate::sql::expr::compare_order_by_values;
use crate::sql::expr::operators::sort_by_fallible;
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::memory::{
    estimate_key_size, estimate_row_size, estimate_value_size, estimate_values_payload_size,
};
use crate::sql::Aggregator;

#[derive(Debug, Clone)]
pub struct AggregateExpr {
    pub func_name: String,
    pub arg: Option<TypedExpr>,
    pub distinct: bool,
    pub delimiter: Option<String>,
    pub filter: Option<TypedExpr>,
    pub order_by: Vec<TypedOrderByExpr>,
}

#[derive(Debug)]
pub struct HashAggregateOperator {
    child: BoxedOperator,
    group_by_exprs: Vec<TypedExpr>,
    aggregate_exprs: Vec<AggregateExpr>,
    output_schema: TableSchema,
    result_rows: Vec<Row>,
    position: usize,
    opened: bool,
}

impl HashAggregateOperator {
    pub fn new(
        child: BoxedOperator,
        group_by_exprs: Vec<TypedExpr>,
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
                generation_expr: None,
                generation_expr_authorized_by: None,
                collation: None,
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
                generation_expr: None,
                generation_expr_authorized_by: None,
                collation: None,
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
            rls_enabled: false,
            rls_force: false,
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

    fn create_aggregator(agg_expr: &AggregateExpr, return_type: &DataType) -> Result<Aggregator> {
        if agg_expr.func_name == "STRING_AGG" {
            let delim = agg_expr.delimiter.as_deref().unwrap_or(",").to_string();
            return Ok(Aggregator::new_string_agg(delim));
        }
        Aggregator::new(&agg_expr.func_name, Some(return_type.clone()))
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
        let input_rows_charged_bytes: usize = input_rows.iter().map(estimate_row_size).sum();

        struct GroupState {
            group_values: Vec<Value>,
            aggregators: Vec<Aggregator>,
            seen_distinct: Vec<HashSet<Vec<u8>>>,
            ordered_agg_buffers: Vec<Option<Vec<(Vec<Value>, Value)>>>,
            charged_bytes: usize,
        }

        let mut groups: HashMap<Vec<u8>, GroupState> = HashMap::new();

        for row in &input_rows {
            let mut group_key_values = Vec::new();
            for expr in &self.group_by_exprs {
                let val = eval_typed_expr(expr, row, ctx.query_ctx)?;
                group_key_values.push(val);
            }

            let key_bytes = encode_values_key(&group_key_values);

            if !groups.contains_key(&key_bytes) {
                let aggregators: Vec<Aggregator> = self
                    .aggregate_exprs
                    .iter()
                    .enumerate()
                    .map(|(i, agg_expr)| {
                        let rt =
                            &self.output_schema.columns[self.group_by_exprs.len() + i].data_type;
                        Self::create_aggregator(agg_expr, rt)
                    })
                    .collect::<Result<Vec<_>>>()?;
                let seen_distinct: Vec<HashSet<Vec<u8>>> = (0..self.aggregate_exprs.len())
                    .map(|_| HashSet::new())
                    .collect();
                let ordered_agg_buffers: Vec<Option<Vec<(Vec<Value>, Value)>>> = self
                    .aggregate_exprs
                    .iter()
                    .map(|agg_expr| {
                        if !agg_expr.order_by.is_empty() {
                            Some(Vec::new())
                        } else {
                            None
                        }
                    })
                    .collect();
                let group_overhead_bytes = std::mem::size_of::<(Vec<u8>, GroupState)>()
                    + key_bytes.len()
                    + estimate_values_payload_size(&group_key_values)
                    + std::mem::size_of_val(aggregators.as_slice())
                    + std::mem::size_of_val(seen_distinct.as_slice())
                    + std::mem::size_of_val(ordered_agg_buffers.as_slice());
                try_grow_statement_memory_scope(
                    "operators.hash_aggregate.groups",
                    group_overhead_bytes,
                )?;
                groups.insert(
                    key_bytes.clone(),
                    GroupState {
                        group_values: group_key_values.clone(),
                        aggregators,
                        seen_distinct,
                        ordered_agg_buffers,
                        charged_bytes: group_overhead_bytes,
                    },
                );
            }

            let state = groups
                .get_mut(&key_bytes)
                .ok_or_else(|| anyhow!("Aggregate group state missing"))?;

            for (i, agg_expr) in self.aggregate_exprs.iter().enumerate() {
                if let Some(ref filter_expr) = agg_expr.filter {
                    let filter_val = eval_typed_expr(filter_expr, row, ctx.query_ctx)?;
                    if !matches!(filter_val, Value::Boolean(true)) {
                        continue;
                    }
                }

                let val = if let Some(arg) = &agg_expr.arg {
                    eval_typed_expr(arg, row, ctx.query_ctx)?
                } else {
                    Value::Int32(1)
                };

                if agg_expr.distinct {
                    let val_bytes = encode_value_key(&val);
                    let distinct_entry_bytes = estimate_key_size(val_bytes.as_slice());
                    if !state.seen_distinct[i].insert(val_bytes) {
                        continue;
                    }
                    try_grow_statement_memory_scope(
                        "operators.hash_aggregate.seen_distinct",
                        distinct_entry_bytes,
                    )?;
                    state.charged_bytes = state.charged_bytes.saturating_add(distinct_entry_bytes);
                }

                if let Some(buf) = state.ordered_agg_buffers[i].as_mut() {
                    let mut keys = Vec::with_capacity(agg_expr.order_by.len());
                    for o in &agg_expr.order_by {
                        let key = eval_typed_expr(&o.expr, row, ctx.query_ctx)?;
                        keys.push(key);
                    }
                    let ordered_entry_bytes = std::mem::size_of::<(Vec<Value>, Value)>()
                        + estimate_values_payload_size(&keys)
                        + estimate_value_size(&val);
                    try_grow_statement_memory_scope(
                        "operators.hash_aggregate.ordered_buffer",
                        ordered_entry_bytes,
                    )?;
                    state.charged_bytes = state.charged_bytes.saturating_add(ordered_entry_bytes);
                    buf.push((keys, val));
                } else {
                    state.aggregators[i].update(&val)?;
                }
            }
        }
        // `collect_all` rows are no longer retained after grouping.
        // Drop first so runtime accounting matches live allocations.
        drop(input_rows);
        try_shrink_statement_memory_scope(input_rows_charged_bytes);

        self.result_rows.clear();

        if groups.is_empty() && self.group_by_exprs.is_empty() {
            let mut values = Vec::new();
            for (i, agg_expr) in self.aggregate_exprs.iter().enumerate() {
                let rt = &self.output_schema.columns[i].data_type;
                let agg = Self::create_aggregator(agg_expr, rt)?;
                values.push(agg.result()?);
            }
            let out_row = Row::new(values);
            try_grow_statement_memory_scope(
                "operators.hash_aggregate.result_rows",
                estimate_row_size(&out_row),
            )?;
            self.result_rows.push(out_row);
        } else {
            for (_, state) in groups {
                let GroupState {
                    group_values,
                    aggregators,
                    mut ordered_agg_buffers,
                    charged_bytes,
                    ..
                } = state;
                let mut values = group_values;
                for (i, mut agg) in aggregators.into_iter().enumerate() {
                    if let Some(mut buf) = ordered_agg_buffers.get_mut(i).and_then(Option::take) {
                        let order_by = &self.aggregate_exprs[i].order_by;
                        sort_by_fallible(&mut buf, |(keys_a, _), (keys_b, _)| {
                            for (key_idx, order_expr) in order_by.iter().enumerate() {
                                let asc = order_expr.asc;
                                let nulls_first = order_expr.nulls_first;
                                let ord = compare_order_by_values(
                                    &keys_a[key_idx],
                                    &keys_b[key_idx],
                                    asc,
                                    nulls_first,
                                )?;
                                if ord != std::cmp::Ordering::Equal {
                                    return Ok(ord);
                                }
                            }
                            Ok(std::cmp::Ordering::Equal)
                        })?;
                        for (_, sorted_value) in buf {
                            agg.update(&sorted_value)?;
                        }
                    }
                    values.push(agg.result()?);
                }
                let out_row = Row::new(values);
                try_grow_statement_memory_scope(
                    "operators.hash_aggregate.result_rows",
                    estimate_row_size(&out_row),
                )?;
                self.result_rows.push(out_row);
                drop(ordered_agg_buffers);
                try_shrink_statement_memory_scope(charged_bytes);
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
        let retained_bytes: usize = self.result_rows.iter().map(estimate_row_size).sum();
        try_shrink_statement_memory_scope(retained_bytes);
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
            .map(|e| format!("{:?}", e))
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
    use crate::sql::analyzer::types::BinaryOp as TypedBinaryOp;
    use crate::sql::analyzer::types::TypedExprKind;
    use crate::sql::operators::scan::TableScanOperator;

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
                    generation_expr: None,
                    generation_expr_authorized_by: None,
                    collation: None,
                },
                ColumnDef {
                    name: "amount".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    generation_expr: None,
                    generation_expr_authorized_by: None,
                    collation: None,
                },
            ],
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
        }
    }

    fn category_ref() -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: 0,
                column_name: "category".to_string(),
            },
            data_type: DataType::Text,
        }
    }

    fn amount_ref() -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: 1,
                column_name: "amount".to_string(),
            },
            data_type: DataType::Int32,
        }
    }

    #[test]
    fn test_hash_aggregate_creation() {
        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let group_by_exprs = vec![category_ref()];
        let aggregate_exprs = vec![AggregateExpr {
            func_name: "SUM".to_string(),
            arg: Some(amount_ref()),
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

        let group_by_exprs = vec![category_ref()];
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
                arg: Some(amount_ref()),
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

        let group_by_exprs = vec![category_ref(), amount_ref()];
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
            arg: Some(category_ref()),
            distinct: false,
            delimiter: Some(", ".to_string()),
            filter: None,
            order_by: vec![],
        };

        let aggregator =
            HashAggregateOperator::create_aggregator(&agg_expr, &DataType::Text).unwrap();
        assert_eq!(aggregator.result().unwrap(), Value::Null);
    }

    #[test]
    fn test_hash_aggregate_with_filter_creation() {
        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let filter_expr = TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(amount_ref()),
                op: TypedBinaryOp::Gt,
                right: Box::new(TypedExpr {
                    kind: TypedExprKind::Constant(Value::Int64(100)),
                    data_type: DataType::Int64,
                }),
            },
            data_type: DataType::Boolean,
        };

        let aggregate_exprs = vec![AggregateExpr {
            func_name: "SUM".to_string(),
            arg: Some(amount_ref()),
            distinct: false,
            delimiter: None,
            filter: Some(filter_expr),
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
