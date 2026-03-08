use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::ops::ControlFlow;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use sqlparser::ast::{
    Expr as AstExpr, Query, SetExpr, TableFactor, TableWithJoins, Value as SqlValue, VisitMut,
    VisitorMut,
};
use tikv_client::Transaction;

use super::helpers::{
    data_type_matches_target, expr_is_target_enum_cast_context, expr_is_target_enum_context,
    is_comparison_op,
};
use super::rename::parse_stored_query;
use crate::model::{ColumnDef, DataType, TableSchema};
use crate::sql::analyzer::{Analyzer, CatalogSnapshot};
use crate::sql::names::normalize_ident;
use crate::storage::TikvStore;

#[derive(Default, Clone)]
pub(super) struct RelationColumnInfo {
    pub(super) all_columns: HashSet<String>,
    pub(super) enum_columns: HashSet<String>,
}

#[derive(Default, Clone)]
pub(super) struct RelationEnumCatalog {
    pub(super) by_full: HashMap<String, RelationColumnInfo>,
}

impl RelationEnumCatalog {
    pub(super) fn insert(&mut self, full_name: String, info: RelationColumnInfo) {
        self.by_full.insert(full_name, info);
    }
}

#[derive(Default, Clone)]
pub(super) struct QueryEnumScope {
    qualified_enum_columns: HashMap<String, HashSet<String>>,
    unqualified_counts: HashMap<String, (u32, u32)>,
}

impl QueryEnumScope {
    pub(super) fn register_relation(&mut self, qualifier: String, relation: &RelationColumnInfo) {
        if !relation.enum_columns.is_empty() {
            self.qualified_enum_columns
                .insert(qualifier, relation.enum_columns.clone());
        }

        for col in &relation.all_columns {
            let entry = self.unqualified_counts.entry(col.clone()).or_insert((0, 0));
            if relation.enum_columns.contains(col) {
                entry.0 += 1;
            } else {
                entry.1 += 1;
            }
        }
    }

    fn is_qualified_enum_column(&self, qualifier: &str, column: &str) -> bool {
        self.qualified_enum_columns
            .get(qualifier)
            .map(|cols| cols.contains(column))
            .unwrap_or(false)
    }

    fn is_unqualified_enum_column(&self, column: &str) -> bool {
        matches!(self.unqualified_counts.get(column), Some((1, 0)))
    }
}

pub(super) async fn build_relation_enum_catalog_from_bindings(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    relation_bindings: &[String],
    enum_full_name: &str,
) -> Result<(RelationEnumCatalog, HashMap<String, TableSchema>)> {
    let mut catalog = RelationEnumCatalog::default();
    let mut relation_schemas: HashMap<String, TableSchema> = HashMap::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut schema_cache: HashMap<String, TableSchema> = HashMap::new();
    let mut resolving: HashSet<String> = HashSet::new();

    for relation_full_name in relation_bindings {
        if !seen.insert(relation_full_name.clone()) {
            continue;
        }

        let Some(schema) = resolve_relation_schema_for_rewrite(
            store,
            txn,
            db_id,
            relation_full_name,
            &mut schema_cache,
            &mut resolving,
        )
        .await?
        else {
            continue;
        };

        let info = relation_column_info_from_schema(&schema, enum_full_name);
        catalog.insert(relation_full_name.clone(), info);
        relation_schemas.insert(relation_full_name.clone(), schema);
    }

    Ok((catalog, relation_schemas))
}

pub(super) fn resolve_relation_schema_for_rewrite<'a>(
    store: &'a Arc<TikvStore>,
    txn: &'a mut Transaction,
    db_id: u64,
    relation_full_name: &'a str,
    schema_cache: &'a mut HashMap<String, TableSchema>,
    resolving: &'a mut HashSet<String>,
) -> Pin<Box<dyn Future<Output = Result<Option<TableSchema>>> + Send + 'a>> {
    Box::pin(async move {
        if let Some(schema) = schema_cache.get(relation_full_name) {
            return Ok(Some(schema.clone()));
        }

        if !resolving.insert(relation_full_name.to_string()) {
            return Err(anyhow!(
                "recursive relation dependency while inferring schema for '{}'",
                relation_full_name
            ));
        }

        let resolved =
            if let Some(table_schema) = store.get_schema(txn, db_id, relation_full_name).await? {
                Some(table_schema)
            } else if let Some(view_def) = store.get_view(txn, db_id, relation_full_name).await? {
                let relation_bindings = store
                    .get_view_relation_bindings(txn, db_id, relation_full_name)
                    .await?
                    .ok_or_else(|| {
                        anyhow!(
                            "missing persisted relation bindings for view '{}'",
                            relation_full_name
                        )
                    })?;
                Some(
                    infer_view_like_schema_for_rewrite(
                        store,
                        txn,
                        db_id,
                        relation_full_name,
                        &view_def.query,
                        &relation_bindings,
                        schema_cache,
                        resolving,
                    )
                    .await?,
                )
            } else if let Some(matview_def) = store
                .get_materialized_view(txn, db_id, relation_full_name)
                .await?
            {
                let relation_bindings = store
                    .get_materialized_view_relation_bindings(txn, db_id, relation_full_name)
                    .await?
                    .ok_or_else(|| {
                        anyhow!(
                            "missing persisted relation bindings for materialized view '{}'",
                            relation_full_name
                        )
                    })?;
                Some(
                    infer_view_like_schema_for_rewrite(
                        store,
                        txn,
                        db_id,
                        relation_full_name,
                        &matview_def.query,
                        &relation_bindings,
                        schema_cache,
                        resolving,
                    )
                    .await?,
                )
            } else {
                None
            };

        resolving.remove(relation_full_name);

        if let Some(schema) = &resolved {
            schema_cache.insert(relation_full_name.to_string(), schema.clone());
        }

        Ok(resolved)
    })
}

async fn infer_view_like_schema_for_rewrite(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    relation_full_name: &str,
    query_sql: &str,
    relation_bindings: &[String],
    schema_cache: &mut HashMap<String, TableSchema>,
    resolving: &mut HashSet<String>,
) -> Result<TableSchema> {
    let query = parse_stored_query(query_sql)?;
    let mut dep_schemas: HashMap<String, TableSchema> = HashMap::new();
    let mut seen: HashSet<String> = HashSet::new();

    for dep in relation_bindings {
        if !seen.insert(dep.clone()) {
            continue;
        }
        let Some(schema) =
            resolve_relation_schema_for_rewrite(store, txn, db_id, dep, schema_cache, resolving)
                .await?
        else {
            return Err(anyhow!(
                "failed to resolve relation schema for bound relation '{}'",
                dep
            ));
        };
        dep_schemas.insert(dep.clone(), schema);
    }

    let mut snapshot =
        build_catalog_snapshot_for_view_bindings(db_id, &query, relation_bindings, &dep_schemas)?;

    // Keep Analyzer behavior aligned with normal execution paths.
    for def in store.list_collations(txn, db_id).await? {
        let name = def.name.clone();
        snapshot.add_collation(&name, def);
    }

    let mut analyzer = Analyzer::new(&snapshot);
    let analyzed = analyzer.analyze_query(&query).map_err(|e| {
        anyhow!(
            "failed to analyze dependency relation '{}' for enum rewrite: {}",
            relation_full_name,
            e
        )
    })?;

    Ok(synthetic_schema_from_output(
        relation_full_name,
        analyzed.output_schema,
    ))
}

pub(super) fn build_catalog_snapshot_for_view_bindings(
    db_id: u64,
    query: &Query,
    relation_bindings: &[String],
    relation_schemas: &HashMap<String, TableSchema>,
) -> Result<CatalogSnapshot> {
    use crate::sql::binder::RelationDep;

    let raw_refs = crate::sql::binder::extract_relation_references_from_query(query);
    if raw_refs.len() != relation_bindings.len() {
        return Err(anyhow!(
            "relation binding count mismatch while rebuilding view catalog: refs={}, bindings={}",
            raw_refs.len(),
            relation_bindings.len()
        ));
    }

    let mut snapshot = CatalogSnapshot::new(vec!["public".to_string()], db_id);
    let mut key_to_full: HashMap<String, String> = HashMap::new();

    let mut bind_key = |key: String, full: &String, schema: &TableSchema| -> Result<()> {
        if let Some(existing) = key_to_full.get(&key) {
            if existing != full {
                return Err(anyhow!(
                    "inconsistent relation binding for key '{}': '{}' vs '{}'",
                    key,
                    existing,
                    full
                ));
            }
            return Ok(());
        }
        key_to_full.insert(key.clone(), full.clone());
        snapshot.add_table(&key, full.clone(), schema.clone());
        Ok(())
    };

    for (raw_ref, resolved_full) in raw_refs.iter().zip(relation_bindings.iter()) {
        let schema = relation_schemas.get(resolved_full).ok_or_else(|| {
            anyhow!(
                "missing relation schema for bound relation '{}'",
                resolved_full
            )
        })?;

        // Always expose the fully-qualified name key.
        bind_key(resolved_full.clone(), resolved_full, schema)?;

        match raw_ref {
            RelationDep::Unqualified { name } => {
                bind_key(name.clone(), resolved_full, schema)?;
            }
            RelationDep::Qualified { schema: s, name } => {
                let key = format!("{}.{}", s, name);
                bind_key(key, resolved_full, schema)?;
            }
        }
    }

    Ok(snapshot)
}

fn synthetic_schema_from_output<C>(
    relation_full_name: &str,
    output_schema: Vec<(String, DataType, Option<C>)>,
) -> TableSchema {
    let columns = output_schema
        .into_iter()
        .map(|(name, data_type, _)| ColumnDef {
            name,
            data_type,
            nullable: true,
            primary_key: false,
            unique: false,
            is_serial: false,
            default_expr: None,
            generation_expr: None,
            generation_expr_authorized_by: None,
            collation: None,
        })
        .collect();

    TableSchema {
        name: relation_full_name.to_string(),
        table_id: 0,
        columns,
        version: 0,
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: "postgres".to_string(),
        from_alias: None,
    }
}

pub(super) fn relation_column_info_from_schema(
    schema: &TableSchema,
    enum_full_name: &str,
) -> RelationColumnInfo {
    let mut info = RelationColumnInfo::default();
    for col in &schema.columns {
        let name = col.name.rsplit('.').next().unwrap_or(&col.name).to_string();
        info.all_columns.insert(name.clone());
        if matches!(&col.data_type, DataType::UserDefined(t) if t == enum_full_name) {
            info.enum_columns.insert(name);
        }
    }
    info
}

pub(super) fn build_query_scope_snapshot(
    query: &Query,
    base_snapshot: &CatalogSnapshot,
) -> Result<CatalogSnapshot> {
    let mut snapshot = base_snapshot.clone();

    let Some(with) = &query.with else {
        return Ok(snapshot);
    };

    for cte in &with.cte_tables {
        let cte_name = normalize_ident(&cte.alias.name);
        let cte_schema_query =
            if with.recursive && crate::sql::executor::cte_is_recursive(&cte.query, &cte_name) {
                let (base_expr, _, _) =
                    crate::sql::executor::decompose_recursive_union(&cte.query, &cte_name)
                        .map_err(|e| {
                            anyhow!(
                    "failed to decompose recursive CTE '{}' while preparing enum rewrite scope: {}",
                    cte_name,
                    e
                )
                        })?;
                Query {
                    with: None,
                    body: base_expr,
                    order_by: vec![],
                    limit: None,
                    offset: None,
                    fetch: None,
                    locks: vec![],
                    limit_by: vec![],
                    for_clause: None,
                }
            } else {
                cte.query.as_ref().clone()
            };
        let mut analyzer = Analyzer::new(&snapshot);
        let analyzed = analyzer.analyze_query(&cte_schema_query).map_err(|e| {
            anyhow!(
                "failed to analyze CTE '{}' while preparing enum rewrite scope: {}",
                cte_name,
                e
            )
        })?;

        let mut output_schema = analyzed.output_schema;
        if !cte.alias.columns.is_empty() {
            if cte.alias.columns.len() != output_schema.len() {
                return Err(anyhow!(
                    "CTE '{}' output column count mismatch while preparing enum rewrite scope: expected {}, got {}",
                    cte_name,
                    cte.alias.columns.len(),
                    output_schema.len()
                ));
            }
            for (idx, alias_col) in cte.alias.columns.iter().enumerate() {
                output_schema[idx].0 = normalize_ident(alias_col);
            }
        }

        let cte_schema = synthetic_schema_from_output(&cte_name, output_schema);
        snapshot.add_table(&cte_name, cte_name.clone(), cte_schema);
        snapshot.mark_non_base(&cte_name);
    }

    Ok(snapshot)
}

fn analyze_query_output_enum_columns(
    query: &Query,
    snapshot: &CatalogSnapshot,
    enum_full_name: &str,
) -> Result<RelationColumnInfo> {
    let mut analyzer = Analyzer::new(snapshot);
    let analyzed = analyzer.analyze_query(query).map_err(|e| {
        anyhow!(
            "failed to analyze derived-table query during enum rewrite: {}",
            e
        )
    })?;
    let mut info = RelationColumnInfo::default();
    for (name, data_type, _collation) in analyzed.output_schema {
        info.all_columns.insert(name.clone());
        if matches!(&data_type, DataType::UserDefined(t) if t == enum_full_name) {
            info.enum_columns.insert(name);
        }
    }
    Ok(info)
}

fn register_derived_aliases_from_table_factor(
    factor: &TableFactor,
    scope: &mut QueryEnumScope,
    snapshot: &CatalogSnapshot,
    enum_full_name: &str,
) -> Result<()> {
    match factor {
        TableFactor::Derived {
            subquery,
            alias: Some(alias),
            ..
        } => {
            let info = analyze_query_output_enum_columns(subquery, snapshot, enum_full_name)?;
            scope.register_relation(normalize_ident(&alias.name), &info);
        }
        TableFactor::NestedJoin {
            table_with_joins, ..
        } => {
            register_derived_aliases_from_table_with_joins(
                table_with_joins,
                scope,
                snapshot,
                enum_full_name,
            )?;
        }
        TableFactor::Pivot { table, .. } | TableFactor::Unpivot { table, .. } => {
            register_derived_aliases_from_table_factor(table, scope, snapshot, enum_full_name)?;
        }
        _ => {}
    }
    Ok(())
}

fn register_derived_aliases_from_table_with_joins(
    table_with_joins: &TableWithJoins,
    scope: &mut QueryEnumScope,
    snapshot: &CatalogSnapshot,
    enum_full_name: &str,
) -> Result<()> {
    register_derived_aliases_from_table_factor(
        &table_with_joins.relation,
        scope,
        snapshot,
        enum_full_name,
    )?;
    for join in &table_with_joins.joins {
        register_derived_aliases_from_table_factor(
            &join.relation,
            scope,
            snapshot,
            enum_full_name,
        )?;
    }
    Ok(())
}

fn register_derived_aliases_for_query(
    query: &Query,
    scope: &mut QueryEnumScope,
    snapshot: &CatalogSnapshot,
    enum_full_name: &str,
) -> Result<()> {
    let snapshot = build_query_scope_snapshot(query, snapshot)?;
    if let SetExpr::Select(select) = query.body.as_ref() {
        for table_with_joins in &select.from {
            register_derived_aliases_from_table_with_joins(
                table_with_joins,
                scope,
                &snapshot,
                enum_full_name,
            )?;
        }
    }
    Ok(())
}

struct QueryEnumLiteralRewriter<'a> {
    scope_stack: Vec<QueryEnumScope>,
    relation_bindings: &'a [String],
    bind_idx: usize,
    query_relation_refs: Vec<Vec<crate::sql::binder::QueryRelationRef>>,
    query_scope_idx: usize,
    catalog: &'a RelationEnumCatalog,
    snapshot: Option<&'a CatalogSnapshot>,
    enum_full_name: &'a str,
    old_label: &'a str,
    new_label: &'a str,
    allow_unqualified_type_match: bool,
    changed: bool,
    error: Option<anyhow::Error>,
}

impl VisitorMut for QueryEnumLiteralRewriter<'_> {
    type Break = ();

    fn pre_visit_query(&mut self, query: &mut Query) -> ControlFlow<Self::Break> {
        let Some(local_refs) = self.query_relation_refs.get(self.query_scope_idx).cloned() else {
            self.error = Some(anyhow!(
                "query relation scope index {} out of bounds while rewriting enum literals (total scopes={})",
                self.query_scope_idx,
                self.query_relation_refs.len()
            ));
            return ControlFlow::Break(());
        };
        self.query_scope_idx += 1;

        let mut scope = QueryEnumScope::default();
        for query_ref in local_refs {
            let Some(bound_full) = self.relation_bindings.get(self.bind_idx) else {
                self.error = Some(anyhow!(
                    "missing relation binding at index {} while rewriting enum literals",
                    self.bind_idx
                ));
                return ControlFlow::Break(());
            };
            self.bind_idx += 1;

            let Some(info) = self.catalog.by_full.get(bound_full) else {
                self.error = Some(anyhow!(
                    "missing relation enum metadata for bound relation '{}'",
                    bound_full
                ));
                return ControlFlow::Break(());
            };

            if !query_ref.qualifier.is_empty() {
                scope.register_relation(query_ref.qualifier, info);
            }
        }

        if let Some(snapshot) = self.snapshot {
            if let Err(e) =
                register_derived_aliases_for_query(query, &mut scope, snapshot, self.enum_full_name)
            {
                self.error = Some(e);
                return ControlFlow::Break(());
            }
        }

        self.scope_stack.push(scope);
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, _query: &mut Query) -> ControlFlow<Self::Break> {
        self.scope_stack.pop();
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, expr: &mut AstExpr) -> ControlFlow<Self::Break> {
        let scope = self.scope_stack.last().cloned().unwrap_or_default();
        if rewrite_enum_literal_in_node_with_scope(
            expr,
            &scope,
            self.enum_full_name,
            self.old_label,
            self.new_label,
            true,
            self.allow_unqualified_type_match,
        ) {
            self.changed = true;
        }
        ControlFlow::Continue(())
    }
}

pub(super) fn rewrite_query_enum_literals_with_catalog(
    query_sql: &str,
    relation_bindings: &[String],
    catalog: &RelationEnumCatalog,
    snapshot: Option<&CatalogSnapshot>,
    enum_full_name: &str,
    old_label: &str,
    new_label: &str,
    allow_unqualified_type_match: bool,
) -> Result<Option<String>> {
    let mut query = parse_stored_query(query_sql)?;
    let query_relation_refs = crate::sql::binder::extract_query_relation_refs_from_query(&query);
    let mut rewriter = QueryEnumLiteralRewriter {
        scope_stack: Vec::new(),
        relation_bindings,
        bind_idx: 0,
        query_relation_refs,
        query_scope_idx: 0,
        catalog,
        snapshot,
        enum_full_name,
        old_label,
        new_label,
        allow_unqualified_type_match,
        changed: false,
        error: None,
    };

    let _ = query.visit(&mut rewriter);
    if let Some(err) = rewriter.error {
        return Err(err);
    }
    if rewriter.bind_idx != relation_bindings.len() {
        return Err(anyhow!(
            "relation bindings not fully consumed during enum rewrite: consumed={}, total={}",
            rewriter.bind_idx,
            relation_bindings.len()
        ));
    }
    if rewriter.query_scope_idx != rewriter.query_relation_refs.len() {
        return Err(anyhow!(
            "query relation scopes not fully consumed during enum rewrite: consumed={}, total={}",
            rewriter.query_scope_idx,
            rewriter.query_relation_refs.len()
        ));
    }

    if rewriter.changed {
        Ok(Some(query.to_string()))
    } else {
        Ok(None)
    }
}

pub(super) async fn rewrite_query_enum_literals_for_view(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    query_sql: &str,
    relation_bindings: &[String],
    enum_full_name: &str,
    old_label: &str,
    new_label: &str,
    allow_unqualified_type_match: bool,
) -> Result<Option<String>> {
    let query = parse_stored_query(query_sql)?;
    let (catalog, relation_schemas) = build_relation_enum_catalog_from_bindings(
        store,
        txn,
        db_id,
        relation_bindings,
        enum_full_name,
    )
    .await?;
    let mut snapshot = build_catalog_snapshot_for_view_bindings(
        db_id,
        &query,
        relation_bindings,
        &relation_schemas,
    )?;
    for def in store.list_collations(txn, db_id).await? {
        let name = def.name.clone();
        snapshot.add_collation(&name, def);
    }
    rewrite_query_enum_literals_with_catalog(
        query_sql,
        relation_bindings,
        &catalog,
        Some(&snapshot),
        enum_full_name,
        old_label,
        new_label,
        allow_unqualified_type_match,
    )
}

pub(super) fn rewrite_expr_enum_literals(
    expr_sql: &str,
    enum_columns: &HashSet<String>,
    enum_full_name: &str,
    old_label: &str,
    new_label: &str,
    allow_column_context: bool,
    allow_unqualified_type_match: bool,
) -> Result<Option<String>> {
    let mut expr = super::rename::parse_sql_expr(expr_sql)?;
    let mut changed = false;

    let _ = sqlparser::ast::visit_expressions_mut(&mut expr, |e| {
        if rewrite_enum_literal_in_node(
            e,
            enum_columns,
            enum_full_name,
            old_label,
            new_label,
            allow_column_context,
            allow_unqualified_type_match,
        ) {
            changed = true;
        }
        ControlFlow::<()>::Continue(())
    });

    if changed {
        Ok(Some(expr.to_string()))
    } else {
        Ok(None)
    }
}

pub(super) fn rewrite_expr_enum_default_for_column(
    expr_sql: &str,
    enum_full_name: &str,
    old_label: &str,
    new_label: &str,
    allow_unqualified_type_match: bool,
) -> Result<Option<String>> {
    let mut expr = super::rename::parse_sql_expr(expr_sql)?;
    let mut changed = rewrite_bare_string_literal(&mut expr, old_label, new_label);
    let empty_cols: HashSet<String> = HashSet::new();

    let _ = sqlparser::ast::visit_expressions_mut(&mut expr, |e| {
        if rewrite_enum_literal_in_node(
            e,
            &empty_cols,
            enum_full_name,
            old_label,
            new_label,
            false,
            allow_unqualified_type_match,
        ) {
            changed = true;
        }
        ControlFlow::<()>::Continue(())
    });

    if changed {
        Ok(Some(expr.to_string()))
    } else {
        Ok(None)
    }
}

fn rewrite_enum_literal_in_node(
    expr: &mut AstExpr,
    enum_columns: &HashSet<String>,
    enum_full_name: &str,
    old_label: &str,
    new_label: &str,
    allow_column_context: bool,
    allow_unqualified_type_match: bool,
) -> bool {
    let mut changed = false;

    match expr {
        AstExpr::Cast {
            expr: inner,
            data_type,
            ..
        }
        | AstExpr::TryCast {
            expr: inner,
            data_type,
            ..
        }
        | AstExpr::SafeCast {
            expr: inner,
            data_type,
            ..
        } => {
            if data_type_matches_target(data_type, enum_full_name, allow_unqualified_type_match) {
                changed |= rewrite_string_literal_expr(inner, old_label, new_label);
            }
        }
        AstExpr::TypedString { data_type, value } => {
            if data_type_matches_target(data_type, enum_full_name, allow_unqualified_type_match)
                && value == old_label
            {
                *value = new_label.to_string();
                changed = true;
            }
        }
        AstExpr::BinaryOp { left, op, right } => {
            if is_comparison_op(op) {
                if allow_column_context
                    && expr_is_target_enum_context(
                        left,
                        enum_columns,
                        enum_full_name,
                        allow_unqualified_type_match,
                    )
                {
                    changed |= rewrite_string_literal_expr(right, old_label, new_label);
                }
                if allow_column_context
                    && expr_is_target_enum_context(
                        right,
                        enum_columns,
                        enum_full_name,
                        allow_unqualified_type_match,
                    )
                {
                    changed |= rewrite_string_literal_expr(left, old_label, new_label);
                }
                if expr_is_target_enum_cast_context(
                    left,
                    enum_full_name,
                    allow_unqualified_type_match,
                ) {
                    changed |= rewrite_string_literal_expr(right, old_label, new_label);
                }
                if expr_is_target_enum_cast_context(
                    right,
                    enum_full_name,
                    allow_unqualified_type_match,
                ) {
                    changed |= rewrite_string_literal_expr(left, old_label, new_label);
                }
            }
        }
        AstExpr::IsDistinctFrom(left, right) | AstExpr::IsNotDistinctFrom(left, right) => {
            if allow_column_context
                && expr_is_target_enum_context(
                    left,
                    enum_columns,
                    enum_full_name,
                    allow_unqualified_type_match,
                )
            {
                changed |= rewrite_string_literal_expr(right, old_label, new_label);
            }
            if allow_column_context
                && expr_is_target_enum_context(
                    right,
                    enum_columns,
                    enum_full_name,
                    allow_unqualified_type_match,
                )
            {
                changed |= rewrite_string_literal_expr(left, old_label, new_label);
            }
            if expr_is_target_enum_cast_context(left, enum_full_name, allow_unqualified_type_match)
            {
                changed |= rewrite_string_literal_expr(right, old_label, new_label);
            }
            if expr_is_target_enum_cast_context(right, enum_full_name, allow_unqualified_type_match)
            {
                changed |= rewrite_string_literal_expr(left, old_label, new_label);
            }
        }
        AstExpr::InList { expr, list, .. } => {
            if allow_column_context
                && expr_is_target_enum_context(
                    expr,
                    enum_columns,
                    enum_full_name,
                    allow_unqualified_type_match,
                )
                || expr_is_target_enum_cast_context(
                    expr,
                    enum_full_name,
                    allow_unqualified_type_match,
                )
            {
                for item in list {
                    changed |= rewrite_string_literal_expr(item, old_label, new_label);
                }
            }
        }
        AstExpr::Between {
            expr, low, high, ..
        } => {
            if allow_column_context
                && expr_is_target_enum_context(
                    expr,
                    enum_columns,
                    enum_full_name,
                    allow_unqualified_type_match,
                )
                || expr_is_target_enum_cast_context(
                    expr,
                    enum_full_name,
                    allow_unqualified_type_match,
                )
            {
                changed |= rewrite_string_literal_expr(low, old_label, new_label);
                changed |= rewrite_string_literal_expr(high, old_label, new_label);
            }
        }
        AstExpr::Case {
            operand: Some(operand),
            conditions,
            ..
        } => {
            if allow_column_context
                && expr_is_target_enum_context(
                    operand,
                    enum_columns,
                    enum_full_name,
                    allow_unqualified_type_match,
                )
                || expr_is_target_enum_cast_context(
                    operand,
                    enum_full_name,
                    allow_unqualified_type_match,
                )
            {
                for cond in conditions {
                    changed |= rewrite_string_literal_expr(cond, old_label, new_label);
                }
            }
        }
        _ => {}
    }

    changed
}

fn rewrite_enum_literal_in_node_with_scope(
    expr: &mut AstExpr,
    scope: &QueryEnumScope,
    enum_full_name: &str,
    old_label: &str,
    new_label: &str,
    allow_column_context: bool,
    allow_unqualified_type_match: bool,
) -> bool {
    let mut changed = false;

    match expr {
        AstExpr::Cast {
            expr: inner,
            data_type,
            ..
        }
        | AstExpr::TryCast {
            expr: inner,
            data_type,
            ..
        }
        | AstExpr::SafeCast {
            expr: inner,
            data_type,
            ..
        } => {
            if data_type_matches_target(data_type, enum_full_name, allow_unqualified_type_match) {
                changed |= rewrite_string_literal_expr(inner, old_label, new_label);
            }
        }
        AstExpr::TypedString { data_type, value } => {
            if data_type_matches_target(data_type, enum_full_name, allow_unqualified_type_match)
                && value == old_label
            {
                *value = new_label.to_string();
                changed = true;
            }
        }
        AstExpr::BinaryOp { left, op, right } => {
            if is_comparison_op(op) {
                if allow_column_context
                    && expr_is_target_enum_context_with_scope(
                        left,
                        scope,
                        enum_full_name,
                        allow_unqualified_type_match,
                    )
                {
                    changed |= rewrite_string_literal_expr(right, old_label, new_label);
                }
                if allow_column_context
                    && expr_is_target_enum_context_with_scope(
                        right,
                        scope,
                        enum_full_name,
                        allow_unqualified_type_match,
                    )
                {
                    changed |= rewrite_string_literal_expr(left, old_label, new_label);
                }
                if expr_is_target_enum_cast_context(
                    left,
                    enum_full_name,
                    allow_unqualified_type_match,
                ) {
                    changed |= rewrite_string_literal_expr(right, old_label, new_label);
                }
                if expr_is_target_enum_cast_context(
                    right,
                    enum_full_name,
                    allow_unqualified_type_match,
                ) {
                    changed |= rewrite_string_literal_expr(left, old_label, new_label);
                }
            }
        }
        AstExpr::IsDistinctFrom(left, right) | AstExpr::IsNotDistinctFrom(left, right) => {
            if allow_column_context
                && expr_is_target_enum_context_with_scope(
                    left,
                    scope,
                    enum_full_name,
                    allow_unqualified_type_match,
                )
            {
                changed |= rewrite_string_literal_expr(right, old_label, new_label);
            }
            if allow_column_context
                && expr_is_target_enum_context_with_scope(
                    right,
                    scope,
                    enum_full_name,
                    allow_unqualified_type_match,
                )
            {
                changed |= rewrite_string_literal_expr(left, old_label, new_label);
            }
            if expr_is_target_enum_cast_context(left, enum_full_name, allow_unqualified_type_match)
            {
                changed |= rewrite_string_literal_expr(right, old_label, new_label);
            }
            if expr_is_target_enum_cast_context(right, enum_full_name, allow_unqualified_type_match)
            {
                changed |= rewrite_string_literal_expr(left, old_label, new_label);
            }
        }
        AstExpr::InList { expr, list, .. } => {
            if allow_column_context
                && expr_is_target_enum_context_with_scope(
                    expr,
                    scope,
                    enum_full_name,
                    allow_unqualified_type_match,
                )
                || expr_is_target_enum_cast_context(
                    expr,
                    enum_full_name,
                    allow_unqualified_type_match,
                )
            {
                for item in list {
                    changed |= rewrite_string_literal_expr(item, old_label, new_label);
                }
            }
        }
        AstExpr::Between {
            expr, low, high, ..
        } => {
            if allow_column_context
                && expr_is_target_enum_context_with_scope(
                    expr,
                    scope,
                    enum_full_name,
                    allow_unqualified_type_match,
                )
                || expr_is_target_enum_cast_context(
                    expr,
                    enum_full_name,
                    allow_unqualified_type_match,
                )
            {
                changed |= rewrite_string_literal_expr(low, old_label, new_label);
                changed |= rewrite_string_literal_expr(high, old_label, new_label);
            }
        }
        AstExpr::Case {
            operand: Some(operand),
            conditions,
            ..
        } => {
            if allow_column_context
                && expr_is_target_enum_context_with_scope(
                    operand,
                    scope,
                    enum_full_name,
                    allow_unqualified_type_match,
                )
                || expr_is_target_enum_cast_context(
                    operand,
                    enum_full_name,
                    allow_unqualified_type_match,
                )
            {
                for cond in conditions {
                    changed |= rewrite_string_literal_expr(cond, old_label, new_label);
                }
            }
        }
        _ => {}
    }

    changed
}

fn expr_is_target_enum_context_with_scope(
    expr: &AstExpr,
    scope: &QueryEnumScope,
    enum_full_name: &str,
    allow_unqualified_type_match: bool,
) -> bool {
    match expr {
        AstExpr::Identifier(ident) => scope.is_unqualified_enum_column(&normalize_ident(ident)),
        AstExpr::CompoundIdentifier(parts) => {
            if parts.len() >= 2 {
                let qualifier = normalize_ident(&parts[parts.len() - 2]);
                let column = normalize_ident(parts.last().expect("parts.len() >= 2"));
                if scope.is_qualified_enum_column(&qualifier, &column) {
                    return true;
                }
                return false;
            }
            parts
                .last()
                .map(normalize_ident)
                .map(|n| scope.is_unqualified_enum_column(&n))
                .unwrap_or(false)
        }
        AstExpr::Nested(inner) | AstExpr::Collate { expr: inner, .. } => {
            expr_is_target_enum_context_with_scope(
                inner,
                scope,
                enum_full_name,
                allow_unqualified_type_match,
            )
        }
        _ => expr_is_target_enum_cast_context(expr, enum_full_name, allow_unqualified_type_match),
    }
}

fn rewrite_bare_string_literal(expr: &mut AstExpr, old_label: &str, new_label: &str) -> bool {
    match expr {
        AstExpr::Value(SqlValue::SingleQuotedString(v)) if v == old_label => {
            *v = new_label.to_string();
            true
        }
        AstExpr::IntroducedString {
            value: SqlValue::SingleQuotedString(v),
            ..
        } if v == old_label => {
            *v = new_label.to_string();
            true
        }
        AstExpr::TypedString { value, .. } if value == old_label => {
            *value = new_label.to_string();
            true
        }
        AstExpr::Nested(inner) | AstExpr::Collate { expr: inner, .. } => {
            rewrite_bare_string_literal(inner, old_label, new_label)
        }
        _ => false,
    }
}

fn rewrite_string_literal_expr(expr: &mut AstExpr, old_label: &str, new_label: &str) -> bool {
    match expr {
        AstExpr::Value(SqlValue::SingleQuotedString(v)) if v == old_label => {
            *v = new_label.to_string();
            true
        }
        AstExpr::IntroducedString {
            value: SqlValue::SingleQuotedString(v),
            ..
        } if v == old_label => {
            *v = new_label.to_string();
            true
        }
        AstExpr::TypedString { value, .. } if value == old_label => {
            *value = new_label.to_string();
            true
        }
        AstExpr::Nested(inner)
        | AstExpr::Collate { expr: inner, .. }
        | AstExpr::UnaryOp { expr: inner, .. }
        | AstExpr::Cast { expr: inner, .. }
        | AstExpr::TryCast { expr: inner, .. }
        | AstExpr::SafeCast { expr: inner, .. } => {
            rewrite_string_literal_expr(inner, old_label, new_label)
        }
        _ => false,
    }
}
