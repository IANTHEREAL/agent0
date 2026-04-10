use super::BuildContext;
use crate::model::{ColumnDef, TableSchema};
use crate::sql::analyzer::types::{AnalyzedProjection, TypedExprKind};
use crate::sql::operators::{BoxedOperator, Db9CopOperator};
use crate::sql::optimizer::logical_plan::PlanSchema;
use crate::sql::optimizer::physical_plan::{Db9CopOp, Db9CopScan};
use anyhow::{anyhow, Result};

pub(super) fn build_db9_cop_operator(
    ctx: &BuildContext,
    plan_schema: &PlanSchema,
    table_name: &str,
    alias: Option<&str>,
    scan: &Db9CopScan,
    ops: &[Db9CopOp],
) -> Result<BoxedOperator> {
    let key = crate::sql::optimizer::schema_map_key(table_name, alias);
    let mut table_schema = ctx
        .table_schemas
        .get(&key)
        .ok_or_else(|| anyhow!("Table schema not found: {}", key))?
        .clone();
    table_schema.from_alias = alias.map(str::to_string);

    Ok(Box::new(Db9CopOperator::new(
        table_schema.clone(),
        output_schema_from_plan(&table_schema, plan_schema, alias, ops),
        scan.clone(),
        ops.to_vec(),
    )))
}

fn output_schema_from_plan(
    table_schema: &TableSchema,
    plan_schema: &PlanSchema,
    alias: Option<&str>,
    ops: &[Db9CopOp],
) -> TableSchema {
    let pushed_projection = ops.iter().find_map(|op| match op {
        Db9CopOp::Project { projections } => Some(projections.as_slice()),
        _ => None,
    });
    let mut output_schema = TableSchema::new(
        table_schema.name.clone(),
        table_schema.table_id,
        plan_schema
            .columns
            .iter()
            .enumerate()
            .map(|(idx, (name, data_type))| {
                output_column_from_plan(idx, name, data_type, table_schema, pushed_projection)
            })
            .collect(),
        vec![],
    );
    output_schema.version = table_schema.version;
    output_schema.from_alias = alias.map(str::to_string);
    output_schema
}

fn output_column_from_plan(
    idx: usize,
    name: &str,
    data_type: &crate::model::DataType,
    table_schema: &TableSchema,
    pushed_projection: Option<&[AnalyzedProjection]>,
) -> ColumnDef {
    let mut column = ColumnDef {
        name: name.to_string(),
        data_type: data_type.clone(),
        nullable: true,
        is_dropped: false,
        primary_key: false,
        unique: false,
        is_serial: false,
        default_expr: None,
        generation_expr: None,
        generation_expr_authorized_by: None,
        collation: None,
    };

    let source_column = match pushed_projection {
        Some(projections) => projections
            .get(idx)
            .and_then(|projection| source_column_for_projection(projection, table_schema)),
        None => table_schema.columns.get(idx),
    };

    if let Some(source_column) = source_column {
        column.nullable = source_column.nullable;
        column.is_serial = source_column.is_serial;
        column.default_expr = source_column.default_expr.clone();
        column.collation = source_column.collation.clone();
    }

    column
}

fn source_column_for_projection<'a>(
    projection: &AnalyzedProjection,
    table_schema: &'a TableSchema,
) -> Option<&'a ColumnDef> {
    match &projection.expr.kind {
        TypedExprKind::ColumnRef {
            scope_depth: 0,
            column_index,
            ..
        } => table_schema.columns.get(*column_index),
        _ => None,
    }
}
