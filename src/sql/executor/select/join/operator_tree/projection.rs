use super::super::super::*;
use super::super::using_merge::{
    build_coalesce_for_merge, rewrite_for_using_join, UsingMergeColumn,
};
use super::TableInfo;
use crate::sql::wildcard::JoinWildcardPlan;

pub(super) fn project_join_output(
    rows: Vec<Row>,
    select: &sqlparser::ast::Select,
    resolved_projection: &[SelectItem],
    final_schema: &TableSchema,
    table_aliases: &[(String, TableSchema)],
    merge_columns: &[UsingMergeColumn],
    wildcard_plan: &Option<JoinWildcardPlan>,
    tables: &[TableInfo],
    source_offsets: &[usize],
) -> Result<(Vec<String>, Vec<DataType>, Vec<Row>)> {
    let has_unqualified_wildcard = resolved_projection
        .iter()
        .any(|p| matches!(p, SelectItem::Wildcard(_)));
    let has_qualified_wildcard = resolved_projection
        .iter()
        .any(|p| matches!(p, SelectItem::QualifiedWildcard(_, _)));
    let has_wildcard = has_unqualified_wildcard || has_qualified_wildcard;

    enum ProjectionSource {
        ColumnIndex(usize),
        Expr(Expr),
        CoalesceColumn(Vec<usize>),
    }

    let (columns, column_types, projected_rows) =
        if has_unqualified_wildcard && wildcard_plan.is_some() && !has_qualified_wildcard {
            let plan = wildcard_plan.as_ref().unwrap();
            let mut cols: Vec<String> = Vec::new();
            let mut types: Vec<DataType> = Vec::new();
            let mut sources: Vec<ProjectionSource> = Vec::new();

            for item in resolved_projection {
                match item {
                    SelectItem::Wildcard(_) => {
                        for wc in &plan.columns {
                            // Guard-rail: wildcard expansion must never leak internal columns (#430),
                            // even if they appear in the plan unexpectedly.
                            let unqualified = wc.name.rsplit('.').next().unwrap_or(&wc.name);
                            if unqualified.starts_with("__tipg_subquery_") {
                                continue;
                            }

                            cols.push(wc.name.clone());
                            types.push(wc.data_type.clone());

                            let mc = merge_columns
                                .iter()
                                .find(|mc| mc.col_name.eq_ignore_ascii_case(&wc.name));
                            let is_merge_source = mc.as_ref().map_or(false, |mc| {
                                let wc_alias = tables
                                    .get(wc.source_idx)
                                    .map(|t| t.alias.as_str())
                                    .unwrap_or("");
                                mc.source_aliases
                                    .iter()
                                    .any(|a| a.eq_ignore_ascii_case(wc_alias))
                            });

                            if let Some(mc) = mc.filter(|_| is_merge_source) {
                                let mut indices: Vec<usize> =
                                    Vec::with_capacity(mc.source_aliases.len());
                                for sa in &mc.source_aliases {
                                    if let Some(ti) =
                                        tables.iter().position(|t| t.alias.eq_ignore_ascii_case(sa))
                                    {
                                        if let Some(ci) = tables[ti]
                                            .schema
                                            .columns
                                            .iter()
                                            .position(|c| c.name.eq_ignore_ascii_case(&wc.name))
                                        {
                                            indices.push(source_offsets[ti] + ci);
                                        }
                                    }
                                }
                                sources.push(ProjectionSource::CoalesceColumn(indices));
                            } else {
                                let idx = source_offsets[wc.source_idx] + wc.col_idx;
                                sources.push(ProjectionSource::ColumnIndex(idx));
                            }
                        }
                    }
                    SelectItem::QualifiedWildcard(_, _) => {
                        return Err(anyhow!(
                            "internal error: expected unqualified wildcard handling only"
                        ));
                    }
                    SelectItem::UnnamedExpr(expr) => {
                        let original_name = get_select_item_name(item);
                        let rewritten = rewrite_for_using_join(expr, table_aliases, merge_columns)?;
                        cols.push(original_name);
                        types.push(infer_expr_type(&rewritten, final_schema));
                        sources.push(ProjectionSource::Expr(rewritten));
                    }
                    SelectItem::ExprWithAlias { expr, alias } => {
                        let rewritten = rewrite_for_using_join(expr, table_aliases, merge_columns)?;
                        cols.push(alias.value.clone());
                        types.push(infer_expr_type(&rewritten, final_schema));
                        sources.push(ProjectionSource::Expr(rewritten));
                    }
                }
            }

            let mut projected: Vec<Row> = Vec::with_capacity(rows.len());
            for row in rows {
                let mut values: Vec<Value> = Vec::with_capacity(sources.len());
                for src in &sources {
                    let val = match src {
                        ProjectionSource::ColumnIndex(idx) => {
                            row.values.get(*idx).cloned().unwrap_or(Value::Null)
                        }
                        ProjectionSource::Expr(expr) => {
                            eval_expr(expr, Some(&row), Some(final_schema))?
                        }
                        ProjectionSource::CoalesceColumn(indices) => {
                            let mut out = Value::Null;
                            for idx in indices {
                                if let Some(val) = row.values.get(*idx) {
                                    if *val != Value::Null {
                                        out = val.clone();
                                        break;
                                    }
                                }
                            }
                            out
                        }
                    };
                    values.push(val);
                }
                projected.push(Row::new(values));
            }

            (cols, types, projected)
        } else if has_wildcard && merge_columns.is_empty() {
            let mut cols: Vec<String> = Vec::new();
            let mut types: Vec<DataType> = Vec::new();
            let mut sources: Vec<ProjectionSource> = Vec::new();

            for item in resolved_projection {
                match item {
                    SelectItem::Wildcard(_) => {
                        for (idx, c) in final_schema.columns.iter().enumerate() {
                            let unqualified = c.name.split('.').last().unwrap_or(&c.name);
                            if unqualified.starts_with("__tipg_subquery_") {
                                continue;
                            }
                            cols.push(c.name.split('.').last().unwrap_or(&c.name).to_string());
                            types.push(c.data_type.clone());
                            sources.push(ProjectionSource::ColumnIndex(idx));
                        }
                    }
                    SelectItem::QualifiedWildcard(obj, _) => {
                        let qualifier = obj.0.last().map(|i| i.value.clone()).unwrap_or_default();
                        let mut matched = false;
                        for (idx, c) in final_schema.columns.iter().enumerate() {
                            let prefix = c.name.split('.').next().unwrap_or(&c.name);
                            if prefix.eq_ignore_ascii_case(&qualifier) {
                                let unqualified = c.name.split('.').last().unwrap_or(&c.name);
                                if unqualified.starts_with("__tipg_subquery_") {
                                    continue;
                                }
                                matched = true;
                                cols.push(c.name.split('.').last().unwrap_or(&c.name).to_string());
                                types.push(c.data_type.clone());
                                sources.push(ProjectionSource::ColumnIndex(idx));
                            }
                        }
                        if !matched {
                            return Err(anyhow!(
                                "Qualified wildcard {}.* not found in join output",
                                qualifier
                            ));
                        }
                    }
                    SelectItem::UnnamedExpr(expr) => {
                        let original_name = get_select_item_name(item);
                        let rewritten = rewrite_for_using_join(expr, table_aliases, merge_columns)?;
                        cols.push(original_name);
                        types.push(infer_expr_type(&rewritten, final_schema));
                        sources.push(ProjectionSource::Expr(rewritten));
                    }
                    SelectItem::ExprWithAlias { expr, alias } => {
                        let rewritten = rewrite_for_using_join(expr, table_aliases, merge_columns)?;
                        cols.push(alias.value.clone());
                        types.push(infer_expr_type(&rewritten, final_schema));
                        sources.push(ProjectionSource::Expr(rewritten));
                    }
                }
            }

            let mut projected: Vec<Row> = Vec::with_capacity(rows.len());
            for row in rows {
                let mut values: Vec<Value> = Vec::with_capacity(sources.len());
                for src in &sources {
                    let val = match src {
                        ProjectionSource::ColumnIndex(idx) => {
                            row.values.get(*idx).cloned().unwrap_or(Value::Null)
                        }
                        ProjectionSource::Expr(expr) => {
                            eval_expr(expr, Some(&row), Some(final_schema))?
                        }
                        ProjectionSource::CoalesceColumn(indices) => {
                            let mut out = Value::Null;
                            for idx in indices {
                                if let Some(val) = row.values.get(*idx) {
                                    if *val != Value::Null {
                                        out = val.clone();
                                        break;
                                    }
                                }
                            }
                            out
                        }
                    };
                    values.push(val);
                }
                projected.push(Row::new(values));
            }

            (cols, types, projected)
        } else if has_qualified_wildcard {
            let mut cols: Vec<String> = Vec::new();
            let mut types: Vec<DataType> = Vec::new();
            let mut sources: Vec<ProjectionSource> = Vec::new();

            for item in resolved_projection {
                match item {
                    SelectItem::Wildcard(_) => {
                        if let Some(plan) = wildcard_plan.as_ref() {
                            for wc in &plan.columns {
                                cols.push(wc.name.clone());
                                types.push(wc.data_type.clone());

                                let mc = merge_columns
                                    .iter()
                                    .find(|mc| mc.col_name.eq_ignore_ascii_case(&wc.name));
                                let is_merge_source = mc.as_ref().map_or(false, |mc| {
                                    let wc_alias = tables
                                        .get(wc.source_idx)
                                        .map(|t| t.alias.as_str())
                                        .unwrap_or("");
                                    mc.source_aliases
                                        .iter()
                                        .any(|a| a.eq_ignore_ascii_case(wc_alias))
                                });

                                if let Some(mc) = mc.filter(|_| is_merge_source) {
                                    let mut indices: Vec<usize> =
                                        Vec::with_capacity(mc.source_aliases.len());
                                    for sa in &mc.source_aliases {
                                        if let Some(ti) = tables
                                            .iter()
                                            .position(|t| t.alias.eq_ignore_ascii_case(sa))
                                        {
                                            if let Some(ci) =
                                                tables[ti].schema.columns.iter().position(|c| {
                                                    c.name.eq_ignore_ascii_case(&wc.name)
                                                })
                                            {
                                                indices.push(source_offsets[ti] + ci);
                                            }
                                        }
                                    }
                                    sources.push(ProjectionSource::CoalesceColumn(indices));
                                } else {
                                    let idx = source_offsets[wc.source_idx] + wc.col_idx;
                                    sources.push(ProjectionSource::ColumnIndex(idx));
                                }
                            }
                        } else {
                            for (idx, c) in final_schema.columns.iter().enumerate() {
                                let unqualified = c.name.split('.').last().unwrap_or(&c.name);
                                if unqualified.starts_with("__tipg_subquery_") {
                                    continue;
                                }
                                cols.push(unqualified.to_string());
                                types.push(c.data_type.clone());
                                sources.push(ProjectionSource::ColumnIndex(idx));
                            }
                        }
                    }
                    SelectItem::QualifiedWildcard(obj, _) => {
                        let qualifier = obj.0.last().map(|i| i.value.clone()).unwrap_or_default();
                        let mut matched = false;
                        for (idx, c) in final_schema.columns.iter().enumerate() {
                            let prefix = c.name.split('.').next().unwrap_or(&c.name);
                            if prefix.eq_ignore_ascii_case(&qualifier) {
                                let unqualified = c.name.split('.').last().unwrap_or(&c.name);
                                if unqualified.starts_with("__tipg_subquery_") {
                                    continue;
                                }
                                matched = true;
                                cols.push(unqualified.to_string());
                                types.push(c.data_type.clone());
                                sources.push(ProjectionSource::ColumnIndex(idx));
                            }
                        }
                        if !matched {
                            return Err(anyhow!(
                                "Qualified wildcard {}.* not found in join output",
                                qualifier
                            ));
                        }
                    }
                    SelectItem::UnnamedExpr(expr) => {
                        let original_name = get_select_item_name(item);
                        let rewritten = rewrite_for_using_join(expr, table_aliases, merge_columns)?;
                        cols.push(original_name);
                        types.push(infer_expr_type(&rewritten, final_schema));
                        sources.push(ProjectionSource::Expr(rewritten));
                    }
                    SelectItem::ExprWithAlias { expr, alias } => {
                        let rewritten = rewrite_for_using_join(expr, table_aliases, merge_columns)?;
                        cols.push(alias.value.clone());
                        types.push(infer_expr_type(&rewritten, final_schema));
                        sources.push(ProjectionSource::Expr(rewritten));
                    }
                }
            }

            let mut projected: Vec<Row> = Vec::with_capacity(rows.len());
            for row in rows {
                let mut values: Vec<Value> = Vec::with_capacity(sources.len());
                for src in &sources {
                    let val = match src {
                        ProjectionSource::ColumnIndex(idx) => {
                            row.values.get(*idx).cloned().unwrap_or(Value::Null)
                        }
                        ProjectionSource::Expr(expr) => {
                            eval_expr(expr, Some(&row), Some(final_schema))?
                        }
                        ProjectionSource::CoalesceColumn(indices) => {
                            let mut out = Value::Null;
                            for idx in indices {
                                if let Some(val) = row.values.get(*idx) {
                                    if *val != Value::Null {
                                        out = val.clone();
                                        break;
                                    }
                                }
                            }
                            out
                        }
                    };
                    values.push(val);
                }
                projected.push(Row::new(values));
            }

            (cols, types, projected)
        } else if has_qualified_wildcard && !has_unqualified_wildcard && !merge_columns.is_empty() {
            let mut cols: Vec<String> = Vec::new();
            let mut types: Vec<DataType> = Vec::new();
            let mut sources: Vec<ProjectionSource> = Vec::new();

            for item in resolved_projection {
                match item {
                    SelectItem::QualifiedWildcard(obj, _) => {
                        let qualifier = obj.0.last().map(|i| i.value.clone()).unwrap_or_default();
                        let (table_idx, table) = tables
                            .iter()
                            .enumerate()
                            .find(|(_, t)| t.alias.eq_ignore_ascii_case(&qualifier))
                            .ok_or_else(|| {
                                anyhow!(
                                    "Qualified wildcard {}.* not found in join output",
                                    qualifier
                                )
                            })?;

                        for (col_idx, col) in table.schema.columns.iter().enumerate() {
                            if col.name.starts_with("__tipg_subquery_") {
                                continue;
                            }

                            cols.push(col.name.clone());
                            types.push(col.data_type.clone());

                            let mc = merge_columns
                                .iter()
                                .find(|mc| mc.col_name.eq_ignore_ascii_case(&col.name));
                            let is_merge_source = mc.as_ref().map_or(false, |mc| {
                                mc.source_aliases
                                    .iter()
                                    .any(|a| a.eq_ignore_ascii_case(&table.alias))
                            });
                            if let Some(mc) = mc.filter(|_| is_merge_source) {
                                sources.push(ProjectionSource::Expr(build_coalesce_for_merge(mc)));
                            } else {
                                let idx = source_offsets[table_idx] + col_idx;
                                sources.push(ProjectionSource::ColumnIndex(idx));
                            }
                        }
                    }
                    SelectItem::UnnamedExpr(expr) => {
                        let original_name = get_select_item_name(item);
                        let rewritten = rewrite_for_using_join(expr, table_aliases, merge_columns)?;
                        cols.push(original_name);
                        types.push(infer_expr_type(&rewritten, final_schema));
                        sources.push(ProjectionSource::Expr(rewritten));
                    }
                    SelectItem::ExprWithAlias { expr, alias } => {
                        let rewritten = rewrite_for_using_join(expr, table_aliases, merge_columns)?;
                        cols.push(alias.value.clone());
                        types.push(infer_expr_type(&rewritten, final_schema));
                        sources.push(ProjectionSource::Expr(rewritten));
                    }
                    SelectItem::Wildcard(_) => {
                        return Err(anyhow!(
                            "internal error: expected qualified wildcard handling only"
                        ));
                    }
                }
            }

            let mut projected: Vec<Row> = Vec::with_capacity(rows.len());
            for row in rows {
                let mut values: Vec<Value> = Vec::with_capacity(sources.len());
                for src in &sources {
                    let val = match src {
                        ProjectionSource::ColumnIndex(idx) => {
                            row.values.get(*idx).cloned().unwrap_or(Value::Null)
                        }
                        ProjectionSource::Expr(expr) => {
                            eval_expr(expr, Some(&row), Some(final_schema))?
                        }
                        ProjectionSource::CoalesceColumn(indices) => {
                            let mut out = Value::Null;
                            for idx in indices {
                                if let Some(val) = row.values.get(*idx) {
                                    if *val != Value::Null {
                                        out = val.clone();
                                        break;
                                    }
                                }
                            }
                            out
                        }
                    };
                    values.push(val);
                }
                projected.push(Row::new(values));
            }

            (cols, types, projected)
        } else if has_unqualified_wildcard {
            let cols: Vec<String> = final_schema
                .columns
                .iter()
                .map(|c| c.name.split('.').last().unwrap_or(&c.name).to_string())
                .collect();
            let types: Vec<DataType> = final_schema
                .columns
                .iter()
                .map(|c| c.data_type.clone())
                .collect();
            (cols, types, rows)
        } else {
            let mut rewritten_projection: Vec<SelectItem> =
                Vec::with_capacity(resolved_projection.len());
            for item in resolved_projection {
                match item {
                    SelectItem::UnnamedExpr(expr) => {
                        let original_name = get_select_item_name(item);
                        let rewritten = rewrite_for_using_join(expr, table_aliases, merge_columns)?;
                        let rewritten_name =
                            get_select_item_name(&SelectItem::UnnamedExpr(rewritten.clone()));
                        if rewritten_name != original_name {
                            rewritten_projection.push(SelectItem::ExprWithAlias {
                                expr: rewritten,
                                alias: Ident::new(original_name),
                            });
                        } else {
                            rewritten_projection.push(SelectItem::UnnamedExpr(rewritten));
                        }
                    }
                    SelectItem::ExprWithAlias { expr, alias } => {
                        rewritten_projection.push(SelectItem::ExprWithAlias {
                            expr: rewrite_for_using_join(expr, table_aliases, merge_columns)?,
                            alias: alias.clone(),
                        });
                    }
                    other => rewritten_projection.push(other.clone()),
                }
            }

            let cols: Vec<String> = rewritten_projection
                .iter()
                .map(|item| get_select_item_name(item))
                .collect();

            let types: Vec<DataType> = rewritten_projection
                .iter()
                .map(|item| match item {
                    SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                        infer_expr_type(expr, final_schema)
                    }
                    _ => DataType::Text,
                })
                .collect();

            fn unnest_arg_expr<'a>(expr: &'a Expr) -> Option<&'a Expr> {
                match expr {
                    Expr::Function(f) => {
                        let Some(name) = f.name.0.last() else {
                            return None;
                        };
                        if !name.value.eq_ignore_ascii_case("UNNEST") {
                            return None;
                        }
                        f.args.first().and_then(|arg| match arg {
                            FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                            _ => None,
                        })
                    }
                    Expr::Nested(inner) => unnest_arg_expr(inner),
                    _ => None,
                }
            }

            let mut projected = Vec::with_capacity(rows.len());
            for row in rows {
                let mut row_values = Vec::with_capacity(rewritten_projection.len());
                let mut srf_outputs: Vec<(usize, Vec<Value>)> = Vec::new();

                for item in &rewritten_projection {
                    let expr = match item {
                        SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => e,
                        _ => continue,
                    };

                    if let Some(arg_expr) = unnest_arg_expr(expr) {
                        let outputs = match eval_expr(arg_expr, Some(&row), Some(final_schema))? {
                            Value::Array(arr) => arr,
                            Value::Null => Vec::new(),
                            other => vec![other],
                        };
                        srf_outputs.push((row_values.len(), outputs));
                        row_values.push(Value::Null);
                        continue;
                    }

                    let val = eval_expr(expr, Some(&row), Some(final_schema))?;
                    row_values.push(val);
                }

                if srf_outputs.is_empty() {
                    projected.push(Row::new(row_values));
                    continue;
                }

                let max_len = srf_outputs
                    .iter()
                    .map(|(_, outputs)| outputs.len())
                    .max()
                    .unwrap_or(0);
                for idx in 0..max_len {
                    let mut expanded = row_values.clone();
                    for (col_idx, outputs) in &srf_outputs {
                        expanded[*col_idx] = outputs.get(idx).cloned().unwrap_or(Value::Null);
                    }
                    projected.push(Row::new(expanded));
                }
            }
            (cols, types, projected)
        };

    let projected_rows = if matches!(&select.distinct, Some(Distinct::Distinct)) {
        dedup_rows(projected_rows)?
    } else {
        projected_rows
    };

    Ok((columns, column_types, projected_rows))
}
