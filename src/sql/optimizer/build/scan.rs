//! Scan operator builders: SeqScan, IndexScan, and LIMIT pushdown.

use std::collections::HashMap;

use anyhow::{anyhow, Result};

use super::BuildContext;
use crate::model::{DataType, TableSchema, Value};
use crate::sql::analyzer::types::TypedExpr;
use crate::sql::operators::{
    BoxedOperator, GinScanOperator, HnswScanOperator, InListScanOperator, IndexScanOperator,
    ProjectOperator, RangeIndexScanOperator, TableScanOperator,
};
use crate::sql::optimizer::physical_plan::{PhysicalNode, PhysicalPlan};
use crate::sql::planner::{collect_typed_eq_predicates, ScanType};
use crate::sql::value_coercion::coerce_value_for_column;

pub(super) fn build_seq_scan_operator(
    ctx: &BuildContext,
    table_name: &str,
    alias: Option<&str>,
    scan_limit: Option<usize>,
) -> Result<BoxedOperator> {
    let key = crate::sql::optimizer::schema_map_key(table_name, alias);
    let schema = ctx
        .table_schemas
        .get(&key)
        .ok_or_else(|| anyhow!("Table schema not found: {}", key))?;
    let mut schema = schema.clone();
    if let Some(a) = alias {
        schema.from_alias = Some(a.to_string());
    }
    // Use preloaded rows for virtual catalog tables, CTEs, etc.
    if let Some(rows) = ctx.preloaded_rows.get(&key) {
        Ok(Box::new(TableScanOperator::new_with_rows(
            schema,
            rows.clone(),
        )))
    } else {
        Ok(Box::new(TableScanOperator::new_with_scan_limit(
            schema, scan_limit,
        )))
    }
}

pub(super) fn build_index_scan_operator(
    ctx: &BuildContext,
    table_name: &str,
    alias: Option<&str>,
    scan_type: &ScanType,
    scan_limit: Option<usize>,
) -> Result<BoxedOperator> {
    let key = crate::sql::optimizer::schema_map_key(table_name, alias);
    let schema = ctx
        .table_schemas
        .get(&key)
        .ok_or_else(|| anyhow!("Table schema not found: {}", key))?;
    let mut schema = schema.clone();
    if let Some(a) = alias {
        schema.from_alias = Some(a.to_string());
    }
    match scan_type {
        ScanType::IndexScan {
            index_id,
            index_name,
            values,
            ..
        } => Ok(Box::new(IndexScanOperator::new_with_scan_limit(
            schema,
            *index_id,
            index_name.clone(),
            values.clone(),
            scan_limit,
        ))),
        ScanType::IndexRangeScan {
            index_id,
            index_name,
            prefix_values,
            ..
        } => Ok(Box::new(RangeIndexScanOperator::new(
            schema,
            *index_id,
            index_name.clone(),
            prefix_values.clone(),
            None,
            true,
            None,
            true,
        ))),
        ScanType::IndexBoundedRangeScan {
            index_id,
            index_name,
            prefix_values,
            range_start,
            start_inclusive,
            range_end,
            end_inclusive,
            ..
        } => Ok(Box::new(RangeIndexScanOperator::new(
            schema,
            *index_id,
            index_name.clone(),
            prefix_values.clone(),
            range_start.clone(),
            *start_inclusive,
            range_end.clone(),
            *end_inclusive,
        ))),
        ScanType::InListScan {
            index_id,
            index_name,
            column_values,
            ..
        } => Ok(Box::new(InListScanOperator::new(
            schema,
            *index_id,
            index_name.clone(),
            column_values.clone(),
        ))),
        ScanType::GinIndexScan {
            index_id,
            index_name,
            qual,
            ..
        } => Ok(Box::new(GinScanOperator::new(
            schema,
            *index_id,
            index_name.clone(),
            qual.clone(),
        ))),
        ScanType::HnswIndexScan {
            index_id,
            index_name,
            query_vector,
            k,
            distance_metric,
            distance_expr,
        } => Ok(Box::new(HnswScanOperator::new(
            schema,
            *index_id,
            index_name.clone(),
            query_vector.clone(),
            *k,
            distance_metric.clone(),
            distance_expr.as_ref().map(|e| *e.clone()),
        ))),
        // FullTableScan should not appear in PhysicalNode::IndexScan.
        other => Err(anyhow!(
            "Unexpected ScanType {:?} in PhysicalNode::IndexScan for table '{}'",
            other,
            table_name
        )),
    }
}

pub(super) fn build_limit_child_with_scan_pushdown(
    input: &PhysicalPlan,
    ctx: &BuildContext,
    scan_limit: usize,
) -> Result<Option<BoxedOperator>> {
    match &input.node {
        PhysicalNode::SeqScan { table_name, alias } => Ok(Some(build_seq_scan_operator(
            ctx,
            table_name,
            alias.as_deref(),
            Some(scan_limit),
        )?)),
        PhysicalNode::IndexScan {
            table_name,
            alias,
            scan_type,
        } => Ok(Some(build_index_scan_operator(
            ctx,
            table_name,
            alias.as_deref(),
            scan_type,
            Some(scan_limit),
        )?)),
        PhysicalNode::HnswScan {
            table_name,
            alias,
            scan_type,
        } => Ok(Some(build_index_scan_operator(
            ctx,
            table_name,
            alias.as_deref(),
            scan_type,
            Some(scan_limit),
        )?)),
        // Projection is row-preserving, so pushing LIMIT through Project is safe.
        PhysicalNode::Project { projections, input } => {
            let Some(child) = build_limit_child_with_scan_pushdown(input, ctx, scan_limit)? else {
                return Ok(None);
            };
            let expressions: Vec<TypedExpr> = projections.iter().map(|p| p.expr.clone()).collect();
            let output_names: Vec<String> =
                projections.iter().map(|p| p.output_name.clone()).collect();
            let output_types: Vec<DataType> = projections
                .iter()
                .map(|p| p.expr.data_type.clone())
                .collect();
            Ok(Some(Box::new(ProjectOperator::new(
                child,
                expressions,
                output_names,
                output_types,
            ))))
        }
        PhysicalNode::Filter { predicate, input } => {
            // Only push through Filter when the Filter is provably redundant with
            // the chosen IndexScan lookup keys (exact equality match on the same
            // leading index columns). Otherwise LIMIT pushdown can change results.
            let PhysicalNode::IndexScan {
                table_name,
                alias,
                scan_type:
                    ScanType::IndexScan {
                        index_id, values, ..
                    },
            } = &input.node
            else {
                return Ok(None);
            };

            let key = crate::sql::optimizer::schema_map_key(table_name, alias.as_deref());
            let Some(schema) = ctx.table_schemas.get(&key) else {
                return Ok(None);
            };
            if !typed_filter_is_exact_index_lookup(predicate, schema, *index_id, values) {
                return Ok(None);
            }

            Ok(Some(build_index_scan_operator(
                ctx,
                table_name,
                alias.as_deref(),
                match &input.node {
                    PhysicalNode::IndexScan { scan_type, .. } => scan_type,
                    _ => unreachable!(),
                },
                Some(scan_limit),
            )?))
        }
        _ => Ok(None),
    }
}

fn typed_filter_is_exact_index_lookup(
    filter: &TypedExpr,
    schema: &TableSchema,
    index_id: u64,
    lookup_values: &[Value],
) -> bool {
    let Some(index) = schema.indexes.iter().find(|i| i.id == index_id) else {
        return false;
    };

    if lookup_values.is_empty() || lookup_values.len() > index.columns.len() {
        return false;
    }

    let mut predicates: HashMap<String, Value> = HashMap::new();
    if collect_typed_eq_predicates(filter, &mut predicates).is_none() {
        return false;
    }
    if predicates.len() != lookup_values.len() {
        return false;
    }

    for (i, col) in index.columns.iter().take(lookup_values.len()).enumerate() {
        let key = col.to_lowercase();
        let Some(pred_value) = predicates.get(&key) else {
            return false;
        };
        let coerced = if let Some(col_def) = schema
            .columns
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(col))
        {
            coerce_value_for_column(pred_value.clone(), col_def)
                .unwrap_or_else(|_| pred_value.clone())
        } else {
            pred_value.clone()
        };
        if coerced != lookup_values[i] {
            return false;
        }
    }

    true
}
