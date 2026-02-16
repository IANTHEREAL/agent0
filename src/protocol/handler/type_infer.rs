//! Query output type inference — resolves column names and types for Describe/RowDescription.
//!
//! This module infers the output schema of SELECT queries, INSERT/UPDATE/DELETE RETURNING
//! clauses, and table-valued functions purely from AST + catalog, without executing the query.
//! Used by the pgwire Describe phase to populate RowDescription messages.

use crate::sql::types::{TypeContext, TypeInferrer};
use crate::sql::Session;
use crate::storage::TikvStore;
use crate::types::{ColumnDef, DataType, TableSchema};
use pgwire::api::results::{FieldFormat, FieldInfo};
use pgwire::api::Type;
use sqlparser::ast::{
    Expr, FunctionArg, FunctionArgExpr, Query, Select, SelectItem, SetExpr, Statement, TableFactor,
    TableWithJoins, Values,
};
use std::collections::HashMap;
use std::sync::Arc;
use tikv_client::Transaction;

use super::encode::datatype_to_pgtype;
use super::schema_resolve::{
    expr_column_name, expr_referenced_column_type, normalize_sql_ident,
    split_object_name_for_catalog,
};
use super::view_infer::{with_view_inference_stack, ViewInferenceGuard};

// ── Table schema resolution ──────────────────────────────────────────────────

pub(super) async fn resolve_table_schema_for_object_name(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    table_name: &sqlparser::ast::ObjectName,
    search_path: &[String],
    is_superuser: bool,
) -> Option<crate::types::TableSchema> {
    let (schema_opt, name) = split_object_name_for_catalog(table_name)?;

    async fn infer_view_schema(
        store: &TikvStore,
        txn: &mut Transaction,
        db_id: u64,
        search_path: &[String],
        full_name: &str,
        is_superuser: bool,
    ) -> Option<crate::types::TableSchema> {
        let view_def = store.get_view(txn, db_id, full_name).await.ok()??;
        let view_query = view_def.query;
        with_view_inference_stack(async move {
            let _guard = ViewInferenceGuard::push(full_name.to_string())?;
            let parsed = crate::sql::parse_sql(&view_query).ok()?;
            let stmt = parsed.into_iter().next()?;
            let Statement::Query(q) = stmt else {
                return None;
            };
            let ctes: HashMap<String, TableSchema> = HashMap::new();
            let cols = infer_query_output_columns_with_txn(
                store,
                txn,
                db_id,
                search_path,
                &q,
                &ctes,
                is_superuser,
            )
            .await?;
            Some(schema_from_inferred_columns(full_name.to_string(), &cols))
        })
        .await
    }

    if let Some(schema) = schema_opt {
        let full = format!("{}.{}", schema, name);
        if let Some(schema) = crate::sql::get_information_schema_schema(&full) {
            return Some(schema);
        }
        if let Some(schema) = Box::pin(infer_view_schema(
            store,
            txn,
            db_id,
            search_path,
            &full,
            is_superuser,
        ))
        .await
        {
            return Some(schema);
        }
        return store.get_schema(txn, db_id, &full).await.ok().flatten();
    }

    if let Some(schema) = crate::sql::get_information_schema_schema(&name) {
        return Some(schema);
    }

    for schema in search_path {
        let full = format!("{}.{}", schema, name);
        if let Some(schema) = Box::pin(infer_view_schema(
            store,
            txn,
            db_id,
            search_path,
            &full,
            is_superuser,
        ))
        .await
        {
            return Some(schema);
        }
        if let Ok(Some(s)) = store.get_schema(txn, db_id, &full).await {
            return Some(s);
        }
    }

    // As a last resort, try the default schema even if it's not present in the session search_path.
    let default_schema = search_path.first().map(String::as_str).unwrap_or("public");
    let full = format!("{}.{}", default_schema, name);
    if let Some(schema) = Box::pin(infer_view_schema(
        store,
        txn,
        db_id,
        search_path,
        &full,
        is_superuser,
    ))
    .await
    {
        return Some(schema);
    }
    store.get_schema(txn, db_id, &full).await.ok().flatten()
}

// ── RETURNING clause inference ───────────────────────────────────────────────

async fn infer_returning_fields_with_txn(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    table_name: &sqlparser::ast::ObjectName,
    returning: &[SelectItem],
    is_superuser: bool,
) -> Option<Vec<FieldInfo>> {
    let schema = resolve_table_schema_for_object_name(
        store,
        txn,
        db_id,
        table_name,
        search_path,
        is_superuser,
    )
    .await?;

    let mut fields = Vec::new();
    for item in returning {
        match item {
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => {
                fields.extend(schema.columns.iter().map(|c| {
                    FieldInfo::new(
                        c.name.clone(),
                        None,
                        None,
                        datatype_to_pgtype(Some(&c.data_type)),
                        FieldFormat::Text,
                    )
                }));
            }
            SelectItem::UnnamedExpr(expr) => {
                let name = expr_column_name(expr).unwrap_or_else(|| "?column?".to_string());
                fields.push(FieldInfo::new(
                    name,
                    None,
                    None,
                    datatype_to_pgtype(expr_referenced_column_type(&schema, expr)),
                    FieldFormat::Text,
                ));
            }
            SelectItem::ExprWithAlias { expr, alias } => {
                fields.push(FieldInfo::new(
                    normalize_sql_ident(alias),
                    None,
                    None,
                    datatype_to_pgtype(expr_referenced_column_type(&schema, expr)),
                    FieldFormat::Text,
                ));
            }
        }
    }

    Some(fields)
}

pub(super) async fn infer_returning_fields_from_statement(
    store: &Arc<TikvStore>,
    session: &mut Session,
    stmt: &Statement,
) -> Option<Vec<FieldInfo>> {
    let (table_name, returning) = match stmt {
        Statement::Insert {
            table_name,
            returning: Some(items),
            ..
        } => (table_name, items),
        Statement::Update {
            table,
            returning: Some(items),
            ..
        } => match &table.relation {
            TableFactor::Table { name, .. } => (name, items),
            _ => return None,
        },
        Statement::Delete {
            from,
            returning: Some(items),
            ..
        } => {
            let first = from.first()?;
            match &first.relation {
                TableFactor::Table { name, .. } => (name, items),
                _ => return None,
            }
        }
        _ => return None,
    };

    let search_path = session.search_path().to_vec();
    let search_path = search_path.as_slice();
    let db_id = session.current_database_id();
    let is_superuser = session.is_superuser();

    if let Some(txn) = session.get_mut_txn() {
        infer_returning_fields_with_txn(
            store.as_ref(),
            txn,
            db_id,
            search_path,
            table_name,
            returning,
            is_superuser,
        )
        .await
    } else {
        let mut temp_txn = store.begin().await.ok()?;
        infer_returning_fields_with_txn(
            store.as_ref(),
            &mut temp_txn,
            db_id,
            search_path,
            table_name,
            returning,
            is_superuser,
        )
        .await
    }
}

// ── Data types and helpers ───────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub(super) struct InferredColumn {
    pub name: String,
    pub data_type: DataType,
}

#[derive(Debug, Clone)]
pub(super) struct SourceSchema {
    pub alias: String,
    pub schema: TableSchema,
}

pub(super) fn stub_describe_field() -> Vec<FieldInfo> {
    vec![FieldInfo::new(
        "column".to_string(),
        None,
        None,
        Type::TEXT,
        FieldFormat::Text,
    )]
}

fn select_item_output_name(item: &SelectItem) -> String {
    match item {
        SelectItem::ExprWithAlias { alias, .. } => alias.value.clone(),
        SelectItem::UnnamedExpr(expr) => expr_output_name(expr),
        SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => "*".to_string(),
    }
}

fn expr_output_name(expr: &Expr) -> String {
    match expr {
        Expr::Identifier(id) => id.value.clone(),
        Expr::CompoundIdentifier(parts) => parts
            .last()
            .map(|p| p.value.clone())
            .unwrap_or_else(|| "?column?".to_string()),
        Expr::Function(f) => {
            if let Some(last_ident) = f.name.0.last() {
                last_ident.value.to_lowercase()
            } else {
                "?column?".to_string()
            }
        }
        Expr::Case { .. } => "case".to_string(),
        Expr::Cast { data_type, .. } => {
            use sqlparser::ast::DataType as SqlDataType;
            match data_type {
                SqlDataType::Int(_) | SqlDataType::Integer(_) => "int4".to_string(),
                SqlDataType::BigInt(_) => "int8".to_string(),
                SqlDataType::SmallInt(_) => "int2".to_string(),
                SqlDataType::Text => "text".to_string(),
                SqlDataType::Varchar(_) | SqlDataType::CharVarying(_) => "varchar".to_string(),
                SqlDataType::Boolean => "bool".to_string(),
                SqlDataType::Float(_) | SqlDataType::Real => "float4".to_string(),
                SqlDataType::Double | SqlDataType::DoublePrecision => "float8".to_string(),
                SqlDataType::Numeric(_) | SqlDataType::Decimal(_) => "numeric".to_string(),
                SqlDataType::Timestamp(_, _) => "timestamp".to_string(),
                SqlDataType::Date => "date".to_string(),
                SqlDataType::Uuid => "uuid".to_string(),
                SqlDataType::JSON => "json".to_string(),
                _ => data_type.to_string().to_lowercase(),
            }
        }
        Expr::Substring { .. } => "substring".to_string(),
        Expr::Trim { .. } => "btrim".to_string(),
        Expr::Position { .. } => "position".to_string(),
        Expr::Extract { .. } => "extract".to_string(),
        Expr::Subquery(_) => "subquery".to_string(),
        Expr::Nested(inner) => expr_output_name(inner),
        _ => "?column?".to_string(),
    }
}

pub(super) fn inferred_columns_to_fields(cols: Vec<InferredColumn>) -> Vec<FieldInfo> {
    cols.into_iter()
        .map(|c| {
            FieldInfo::new(
                c.name,
                None,
                None,
                datatype_to_pgtype(Some(&c.data_type)),
                FieldFormat::Text,
            )
        })
        .collect()
}

fn schema_from_inferred_columns(name: String, cols: &[InferredColumn]) -> TableSchema {
    let columns = cols
        .iter()
        .map(|c| ColumnDef {
            name: c.name.clone(),
            data_type: c.data_type.clone(),
            nullable: true,
            primary_key: false,
            unique: false,
            is_serial: false,
            default_expr: None,
        })
        .collect();
    TableSchema::new(name, 0, columns, Vec::new())
}

fn apply_column_aliases(schema: &mut TableSchema, aliases: &[sqlparser::ast::Ident]) {
    for (idx, ident) in aliases.iter().enumerate() {
        if let Some(col) = schema.columns.get_mut(idx) {
            col.name = normalize_sql_ident(ident);
        }
    }
}

fn base_table_name(full: &str) -> &str {
    full.rsplit('.').next().unwrap_or(full)
}

fn infer_system_table_function_schema(func_name: &str) -> Option<TableSchema> {
    crate::sql::catalog::virtual_tables::virtual_table_schema(func_name)
}

#[cfg(test)]
pub(super) async fn infer_fs9_table_function_schema(
    args: &[FunctionArg],
    is_superuser: bool,
) -> Option<TableSchema> {
    crate::sql::table_functions::infer_fs9_table_function_schema(args, is_superuser).await
}

// ── Query output inference ───────────────────────────────────────────────────

async fn infer_query_output_columns(
    store: &Arc<TikvStore>,
    session: &mut Session,
    query: &Query,
) -> Option<Vec<InferredColumn>> {
    let search_path = session.search_path().to_vec();
    let search_path = search_path.as_slice();
    let db_id = session.current_database_id();
    let is_superuser = session.is_superuser();
    let outer_ctes: HashMap<String, TableSchema> = HashMap::new();

    if let Some(txn) = session.get_mut_txn() {
        infer_query_output_columns_with_txn(
            store.as_ref(),
            txn,
            db_id,
            search_path,
            query,
            &outer_ctes,
            is_superuser,
        )
        .await
    } else {
        let mut temp_txn = store.begin().await.ok()?;
        let cols = infer_query_output_columns_with_txn(
            store.as_ref(),
            &mut temp_txn,
            db_id,
            search_path,
            query,
            &outer_ctes,
            is_superuser,
        )
        .await;
        let _ = temp_txn.rollback().await;
        cols
    }
}

async fn build_cte_schemas_with_txn(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    query: &Query,
    outer_ctes: &HashMap<String, TableSchema>,
    is_superuser: bool,
) -> Option<HashMap<String, TableSchema>> {
    let with = query.with.as_ref()?;
    if with.recursive {
        return None;
    }

    let mut ctes = outer_ctes.clone();
    for cte in &with.cte_tables {
        let cte_name = normalize_sql_ident(&cte.alias.name);
        let mut cols = Box::pin(infer_query_output_columns_with_txn(
            store,
            txn,
            db_id,
            search_path,
            &cte.query,
            &ctes,
            is_superuser,
        ))
        .await?;

        if !cte.alias.columns.is_empty() {
            for (idx, ident) in cte.alias.columns.iter().enumerate() {
                if let Some(col) = cols.get_mut(idx) {
                    col.name = normalize_sql_ident(ident);
                }
            }
        }

        ctes.insert(
            cte_name.clone(),
            schema_from_inferred_columns(cte_name, &cols),
        );
    }
    Some(ctes)
}

async fn infer_query_output_columns_with_txn(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    query: &Query,
    outer_ctes: &HashMap<String, TableSchema>,
    is_superuser: bool,
) -> Option<Vec<InferredColumn>> {
    let ctes = build_cte_schemas_with_txn(
        store,
        txn,
        db_id,
        search_path,
        query,
        outer_ctes,
        is_superuser,
    )
    .await
    .unwrap_or_else(|| outer_ctes.clone());

    infer_setexpr_output_columns_with_txn(
        store,
        txn,
        db_id,
        search_path,
        &query.body,
        &ctes,
        is_superuser,
    )
    .await
}

async fn infer_setexpr_output_columns_with_txn(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    body: &SetExpr,
    ctes: &HashMap<String, TableSchema>,
    is_superuser: bool,
) -> Option<Vec<InferredColumn>> {
    match body {
        SetExpr::Select(select) => {
            infer_select_output_columns_with_txn(
                store,
                txn,
                db_id,
                search_path,
                select,
                ctes,
                is_superuser,
            )
            .await
        }
        SetExpr::Values(values) => infer_values_output_columns(values),
        SetExpr::SetOperation { left, .. } => {
            Box::pin(infer_setexpr_output_columns_with_txn(
                store,
                txn,
                db_id,
                search_path,
                left,
                ctes,
                is_superuser,
            ))
            .await
        }
        _ => None,
    }
}

fn infer_values_output_columns(values: &Values) -> Option<Vec<InferredColumn>> {
    let first = values.rows.first()?;
    let ctx = TypeContext::empty();
    let mut inferrer = TypeInferrer::new(ctx);

    Some(
        first
            .iter()
            .enumerate()
            .map(|(idx, expr)| InferredColumn {
                name: format!("column{}", idx + 1),
                // INTENTIONAL: wire protocol encoding — Text OID is universally safe
                data_type: inferrer.infer(expr).unwrap_or(DataType::Text),
            })
            .collect(),
    )
}

// ── Source collection (FROM clause) ──────────────────────────────────────────

async fn collect_sources_from_table_with_joins(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    twj: &TableWithJoins,
    ctes: &HashMap<String, TableSchema>,
    out: &mut Vec<SourceSchema>,
    is_superuser: bool,
) -> Option<()> {
    collect_sources_from_table_factor(
        store,
        txn,
        db_id,
        search_path,
        &twj.relation,
        ctes,
        out,
        is_superuser,
    )
    .await?;
    for join in &twj.joins {
        collect_sources_from_table_factor(
            store,
            txn,
            db_id,
            search_path,
            &join.relation,
            ctes,
            out,
            is_superuser,
        )
        .await?;
    }
    Some(())
}

async fn collect_sources_from_table_factor(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    factor: &TableFactor,
    ctes: &HashMap<String, TableSchema>,
    out: &mut Vec<SourceSchema>,
    is_superuser: bool,
) -> Option<()> {
    fn infer_generate_series_schema(
        args: &[FunctionArg],
        alias_name: &str,
        table_alias: Option<&sqlparser::ast::TableAlias>,
    ) -> Option<TableSchema> {
        if args.len() < 2 {
            return None;
        }

        fn extract_expr(arg: &FunctionArg) -> Option<&Expr> {
            match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => Some(expr),
                FunctionArg::Named {
                    arg: FunctionArgExpr::Expr(expr),
                    ..
                } => Some(expr),
                _ => None,
            }
        }

        let start_expr = extract_expr(&args[0])?;
        let stop_expr = extract_expr(&args[1])?;

        let mut inferrer = TypeInferrer::new(TypeContext::empty());
        let start_type = inferrer.infer(start_expr).ok()?;
        let stop_type = inferrer.infer(stop_expr).ok()?;

        let data_type = match (&start_type, &stop_type) {
            (DataType::Int32, DataType::Int32) => DataType::Int32,
            (DataType::Int64, DataType::Int64) => DataType::Int64,
            (DataType::Int32, DataType::Int64) | (DataType::Int64, DataType::Int32) => {
                DataType::Int64
            }
            (DataType::Float64, DataType::Float64) => DataType::Float64,
            (DataType::Numeric { .. }, DataType::Numeric { .. }) => DataType::Numeric {
                precision: None,
                scale: None,
            },
            (DataType::Date, DataType::Date) => DataType::TimestampTz,
            (DataType::Timestamp, DataType::Timestamp)
            | (DataType::TimestampTz, DataType::Timestamp)
            | (DataType::Timestamp, DataType::TimestampTz)
            | (DataType::TimestampTz, DataType::TimestampTz) => DataType::Timestamp,
            _ => DataType::Text,
        };

        let col_name = if let Some(ta) = table_alias {
            if !ta.columns.is_empty() {
                normalize_sql_ident(&ta.columns[0])
            } else {
                alias_name.to_string()
            }
        } else {
            "generate_series".to_string()
        };

        Some(TableSchema {
            table_id: 0,
            name: "generate_series".to_string(),
            columns: vec![ColumnDef {
                name: col_name,
                data_type,
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            }],
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            version: 1,
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        })
    }

    match factor {
        TableFactor::Table {
            name, alias, args, ..
        } => {
            let (schema_opt, obj_name_norm) = split_object_name_for_catalog(name)?;
            let alias_name = alias
                .as_ref()
                .map(|a| a.name.value.clone())
                .unwrap_or_else(|| name.0.last().map(|i| i.value.clone()).unwrap_or_default());
            if alias_name.is_empty() {
                return None;
            }

            let mut schema = if let Some(args) = args {
                if obj_name_norm.eq_ignore_ascii_case("generate_series") {
                    infer_generate_series_schema(args, &alias_name, alias.as_ref())?
                } else if let Some(sys_schema) = infer_system_table_function_schema(&obj_name_norm)
                {
                    sys_schema
                } else {
                    crate::sql::table_functions::infer_extension_table_function_schema(
                        search_path,
                        schema_opt.as_deref(),
                        &obj_name_norm,
                        args,
                        is_superuser,
                    )
                    .await?
                }
            } else if schema_opt.is_none() {
                match ctes.get(&obj_name_norm) {
                    Some(cte_schema) => cte_schema.clone(),
                    None => {
                        resolve_table_schema_for_object_name(
                            store,
                            txn,
                            db_id,
                            name,
                            search_path,
                            is_superuser,
                        )
                        .await?
                    }
                }
            } else {
                resolve_table_schema_for_object_name(
                    store,
                    txn,
                    db_id,
                    name,
                    search_path,
                    is_superuser,
                )
                .await?
            };

            if let Some(alias) = alias {
                if !alias.columns.is_empty() {
                    apply_column_aliases(&mut schema, &alias.columns);
                }
            }

            out.push(SourceSchema {
                alias: alias_name,
                schema,
            });
            Some(())
        }
        TableFactor::Derived {
            subquery, alias, ..
        } => {
            let alias = alias.as_ref()?;
            let alias_name = alias.name.value.clone();
            if alias_name.is_empty() {
                return None;
            }

            let cols = infer_query_output_columns_with_txn(
                store,
                txn,
                db_id,
                search_path,
                subquery.as_ref(),
                ctes,
                is_superuser,
            );
            let mut cols = Box::pin(cols).await?;

            if !alias.columns.is_empty() {
                for (idx, ident) in alias.columns.iter().enumerate() {
                    if let Some(col) = cols.get_mut(idx) {
                        col.name = normalize_sql_ident(ident);
                    }
                }
            }

            out.push(SourceSchema {
                alias: alias_name.clone(),
                schema: schema_from_inferred_columns(alias_name, &cols),
            });
            Some(())
        }
        _ => None,
    }
}

// ── SELECT output inference ──────────────────────────────────────────────────

async fn infer_select_output_columns_with_txn(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    select: &Select,
    ctes: &HashMap<String, TableSchema>,
    is_superuser: bool,
) -> Option<Vec<InferredColumn>> {
    let mut sources = Vec::new();
    for twj in &select.from {
        collect_sources_from_table_with_joins(
            store,
            txn,
            db_id,
            search_path,
            twj,
            ctes,
            &mut sources,
            is_superuser,
        )
        .await?;
    }

    let join_wildcard_plan = if sources.is_empty() {
        None
    } else {
        let schema_refs: Vec<&TableSchema> = sources.iter().map(|s| &s.schema).collect();
        crate::sql::wildcard::build_join_wildcard_plan(select, &schema_refs)
    };

    let mut ctx = TypeContext::empty();
    for src in &sources {
        ctx.add_table(&src.alias, &src.schema);
    }
    let mut inferrer = TypeInferrer::new(ctx);

    let mut out_cols = Vec::new();
    for item in &select.projection {
        match item {
            SelectItem::Wildcard(_) => {
                if sources.is_empty() {
                    return None;
                }
                if let Some(plan) = join_wildcard_plan.as_ref().filter(|p| p.any_merge) {
                    out_cols.extend(plan.columns.iter().map(|c| InferredColumn {
                        name: c.name.clone(),
                        data_type: c.data_type.clone(),
                    }));
                } else {
                    for src in &sources {
                        out_cols.extend(src.schema.columns.iter().map(|c| InferredColumn {
                            name: c.name.clone(),
                            data_type: c.data_type.clone(),
                        }));
                    }
                }
            }
            SelectItem::QualifiedWildcard(obj, _) => {
                if sources.is_empty() {
                    return None;
                }
                let target = obj.0.last().map(|i| i.value.as_str())?;
                let src = sources.iter().find(|s| {
                    s.alias.eq_ignore_ascii_case(target)
                        || base_table_name(&s.schema.name).eq_ignore_ascii_case(target)
                })?;
                out_cols.extend(src.schema.columns.iter().map(|c| InferredColumn {
                    name: c.name.clone(),
                    data_type: c.data_type.clone(),
                }));
            }
            SelectItem::UnnamedExpr(expr) => {
                out_cols.push(InferredColumn {
                    name: select_item_output_name(item),
                    // INTENTIONAL: wire protocol encoding — Text OID is universally safe
                    data_type: inferrer.infer(expr).unwrap_or(DataType::Text),
                });
            }
            SelectItem::ExprWithAlias { expr, alias } => {
                out_cols.push(InferredColumn {
                    name: alias.value.clone(),
                    // INTENTIONAL: wire protocol encoding — Text OID is universally safe
                    data_type: inferrer.infer(expr).unwrap_or(DataType::Text),
                });
            }
        }
    }

    Some(out_cols)
}

// ── Top-level entry point ────────────────────────────────────────────────────

pub(super) async fn infer_result_fields_from_query_ast(
    store: &Arc<TikvStore>,
    session: &mut Session,
    query: &str,
) -> Vec<FieldInfo> {
    let query_trimmed = query.trim();
    if query_trimmed.is_empty() {
        return vec![];
    }

    let parsed_stmt = crate::sql::parse_sql(query_trimmed)
        .ok()
        .and_then(|stmts| stmts.into_iter().next());
    let query_upper = query_trimmed.to_uppercase();

    let is_select_str = query_upper.starts_with("SELECT") || query_upper.starts_with("WITH");
    let has_returning_str = query_upper.contains("RETURNING");
    let should_infer = matches!(
        parsed_stmt,
        Some(Statement::Query(_))
            | Some(Statement::Insert {
                returning: Some(_),
                ..
            })
            | Some(Statement::Update {
                returning: Some(_),
                ..
            })
            | Some(Statement::Delete {
                returning: Some(_),
                ..
            })
    ) || (parsed_stmt.is_none() && (is_select_str || has_returning_str));

    if !should_infer {
        return vec![];
    }

    if let Some(ref stmt) = parsed_stmt {
        if let Some(fields) = infer_returning_fields_from_statement(store, session, stmt).await {
            return fields;
        }
    }

    // Only infer SELECT (Statement::Query) metadata here; RETURNING is handled above.
    let is_select = matches!(parsed_stmt, Some(Statement::Query(_)))
        || (parsed_stmt.is_none() && is_select_str);
    if !is_select {
        return stub_describe_field();
    }

    let stmt = match parsed_stmt {
        Some(stmt) => stmt,
        None => return stub_describe_field(),
    };

    match stmt {
        Statement::Query(q) => match infer_query_output_columns(store, session, &q).await {
            Some(cols) => inferred_columns_to_fields(cols),
            None => stub_describe_field(),
        },
        _ => stub_describe_field(),
    }
}
