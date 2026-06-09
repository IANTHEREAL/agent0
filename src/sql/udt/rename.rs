use std::ops::ControlFlow;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use sqlparser::ast::{
    visit_expressions_mut, DataType as SqlDataType, Expr as AstExpr, Ident, ObjectName, Query,
    Statement,
};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use tikv_client::Transaction;

use super::helpers::{expr_has_unqualified_type_cast, query_has_unqualified_type_cast};
use crate::model::{build_predicate_conjunct_cache, DataType, TableSchema, UserTypeKind};
use crate::sql::ddl::{check_relation_name_available, RelationKind};
use crate::sql::names;
use crate::sql::names::normalize_ident;
use crate::sql::ExecuteResult;
use crate::storage::TikvStore;

/// Fetch the enum type definition, returning an error if it doesn't exist or
/// is not an enum.
pub(super) async fn get_enum_type(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    full_name: &str,
) -> Result<crate::model::UserTypeDef> {
    let def = store
        .get_type(txn, db_id, full_name)
        .await?
        .ok_or_else(|| anyhow!("type \"{}\" does not exist", full_name))?;
    if !matches!(def.kind, UserTypeKind::Enum { .. }) {
        return Err(anyhow!(
            "\"{}\" is not an enum type",
            full_name.rsplit('.').next().unwrap_or(full_name)
        ));
    }
    Ok(def)
}

pub(super) async fn can_match_unqualified_type_name(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    target_full: &str,
) -> Result<bool> {
    let (_, target_name) = names::parse_full_name(target_full)?;
    let mut found_target = false;

    for ty in store.list_types(txn, db_id).await? {
        if ty.name != target_name {
            continue;
        }
        let full = format!("{}.{}", ty.schema, ty.name);
        if full == target_full {
            found_target = true;
            continue;
        }
        return Ok(false);
    }

    Ok(found_target)
}

async fn ensure_no_ambiguous_unqualified_casts_for_rename(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    old_full: &str,
) -> Result<()> {
    let (_, old_name) = names::parse_full_name(old_full)?;
    let ambiguous_err = |location: &str| -> anyhow::Error {
        anyhow!(
            "cannot rename type \"{}\" because unqualified cast \"::{}\" is ambiguous across schemas\nDETAIL:  Found in {}.\nHINT:  Qualify casts with schema (for example, \"::schema.{}\") before renaming.",
            old_full,
            old_name,
            location,
            old_name
        )
    };

    for table_name in store.list_tables(txn, db_id).await? {
        let Some(schema) = store.get_schema(txn, db_id, &table_name).await? else {
            continue;
        };

        for col in &schema.columns {
            if let Some(expr_sql) = &col.default_expr {
                if expr_has_unqualified_type_cast(expr_sql, &old_name)? {
                    return Err(ambiguous_err(&format!(
                        "DEFAULT expression for column '{}' of table '{}'",
                        col.name, table_name
                    )));
                }
            }
        }
        for check in &schema.check_constraints {
            if expr_has_unqualified_type_cast(&check.expr, &old_name)? {
                let check_name = check.name.as_deref().unwrap_or("<unnamed>");
                return Err(ambiguous_err(&format!(
                    "CHECK constraint '{}' of table '{}'",
                    check_name, table_name
                )));
            }
        }
        for idx in &schema.indexes {
            if let Some(pred) = &idx.predicate {
                if expr_has_unqualified_type_cast(pred, &old_name)? {
                    return Err(ambiguous_err(&format!(
                        "index predicate '{}' of table '{}'",
                        idx.name, table_name
                    )));
                }
            }
            for expr_sql in &idx.expressions {
                if expr_has_unqualified_type_cast(expr_sql, &old_name)? {
                    return Err(ambiguous_err(&format!(
                        "index expression '{}' of table '{}'",
                        idx.name, table_name
                    )));
                }
            }
        }
    }

    for view in store.list_views(txn, db_id).await? {
        let full_name = view.full_name();
        if query_has_unqualified_type_cast(&view.query, &old_name)? {
            return Err(ambiguous_err(&format!("view '{}'", full_name)));
        }
    }

    for matview in store.list_materialized_views(txn, db_id).await? {
        let full_name = matview.full_name();
        if query_has_unqualified_type_cast(&matview.query, &old_name)? {
            return Err(ambiguous_err(&format!("materialized view '{}'", full_name)));
        }
    }

    Ok(())
}

/// `ALTER TYPE <name> RENAME TO <new_name>`
///
/// Renames the type key in storage and updates all table column schemas that
/// reference the old name via `DataType::UserDefined(old_full)`.
pub async fn alter_type_rename(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    old_full: &str,
    new_name: &str,
) -> Result<ExecuteResult> {
    if new_name.is_empty() {
        return Err(anyhow!("zero-length delimited identifier"));
    }

    let def = store
        .get_type(txn, db_id, old_full)
        .await?
        .ok_or_else(|| anyhow!("type \"{}\" does not exist", old_full))?;

    let schema_part = old_full.split('.').next().unwrap_or("public");
    let new_full = format!("{}.{}", schema_part, new_name);

    // Unified namespace check: reserve the new name before renaming.
    check_relation_name_available(
        store,
        txn,
        db_id,
        schema_part,
        new_name,
        RelationKind::Type,
        false,
        None,
    )
    .await?;

    // Single-part type names (`::mood`) are only rewritten when that bare name
    // maps uniquely to the renamed type in this database. This prevents
    // cross-schema mis-rewrites when multiple schemas define the same type name.
    let allow_unqualified_type_match =
        can_match_unqualified_type_name(store, txn, db_id, old_full).await?;
    if !allow_unqualified_type_match {
        ensure_no_ambiguous_unqualified_casts_for_rename(store, txn, db_id, old_full).await?;
    }

    store
        .rename_type(txn, db_id, old_full, &new_full, def)
        .await?;
    // Release the old name's reservation key (no-op if missing).
    store.release_relation_name(txn, db_id, old_full).await?;

    // Update all table schemas that reference the old type name.
    // This covers DataType::UserDefined references AND SQL string artifacts
    // containing cast expressions like `::old_name` or `::schema.old_name`.
    let tables = store.list_tables(txn, db_id).await?;
    for table_name in tables {
        let Some(mut schema) = store.get_schema(txn, db_id, &table_name).await? else {
            continue;
        };
        let mut changed = false;
        for col in &mut schema.columns {
            if matches!(&col.data_type, DataType::UserDefined(t) if t == old_full) {
                col.data_type = DataType::UserDefined(new_full.clone());
                changed = true;
            }
        }
        // Update cast references (e.g., 'red'::color → 'red'::palette) in
        // defaults, check constraints, and index predicates/expressions.
        if update_schema_type_casts(
            &mut schema,
            old_full,
            &new_full,
            allow_unqualified_type_match,
        )? {
            changed = true;
        }
        if changed {
            schema.version += 1;
            store.update_schema(txn, db_id, schema).await?;
        }
    }

    // Views/matviews store query text and are re-parsed at query time. Keep
    // dependent definitions valid by rewriting cast targets there as well.
    for view in store.list_views(txn, db_id).await? {
        let full_name = view.full_name();
        if let Some(rewritten) = rewrite_query_type_casts(
            &view.query,
            old_full,
            &new_full,
            allow_unqualified_type_match,
        )? {
            store
                .update_view_query(txn, db_id, &full_name, &rewritten)
                .await?;
        }
    }
    for matview in store.list_materialized_views(txn, db_id).await? {
        let full_name = matview.full_name();
        if let Some(rewritten) = rewrite_query_type_casts(
            &matview.query,
            old_full,
            &new_full,
            allow_unqualified_type_match,
        )? {
            store
                .update_materialized_view_query(txn, db_id, &full_name, &rewritten)
                .await?;
        }
    }

    Ok(ExecuteResult::CommandComplete { tag: "ALTER TYPE" })
}

/// Update type cast references in SQL string artifacts when a type is renamed.
///
/// Rewrites cast target datatypes through SQL AST instead of string replacement.
/// Returns `true` if any changes were made.
pub(super) fn update_schema_type_casts(
    schema: &mut TableSchema,
    old_full: &str,
    new_full: &str,
    allow_unqualified_type_match: bool,
) -> Result<bool> {
    let mut changed = false;

    for col in &mut schema.columns {
        if let Some(ref expr) = col.default_expr {
            if let Some(rewritten) =
                rewrite_expr_type_casts(expr, old_full, new_full, allow_unqualified_type_match)?
            {
                col.default_expr = Some(rewritten);
                changed = true;
            }
        }
    }
    for check in &mut schema.check_constraints {
        if let Some(rewritten) = rewrite_expr_type_casts(
            &check.expr,
            old_full,
            new_full,
            allow_unqualified_type_match,
        )? {
            check.expr = rewritten;
            changed = true;
        }
    }
    for idx in &mut schema.indexes {
        if let Some(ref pred) = idx.predicate {
            if let Some(rewritten) =
                rewrite_expr_type_casts(pred, old_full, new_full, allow_unqualified_type_match)?
            {
                idx.predicate = Some(rewritten);
                idx.cached_predicate_conjuncts =
                    build_predicate_conjunct_cache(idx.predicate.as_deref());
                changed = true;
            }
        }
        for expr_str in &mut idx.expressions {
            if let Some(rewritten) =
                rewrite_expr_type_casts(expr_str, old_full, new_full, allow_unqualified_type_match)?
            {
                *expr_str = rewritten;
                changed = true;
            }
        }
    }
    Ok(changed)
}

fn rewrite_query_type_casts(
    query_sql: &str,
    old_full: &str,
    new_full: &str,
    allow_unqualified_type_match: bool,
) -> Result<Option<String>> {
    let mut query = parse_stored_query(query_sql)?;
    let (old_schema, old_name) = names::parse_full_name(old_full)?;
    let (new_schema, new_name) = names::parse_full_name(new_full)?;
    let mut changed = false;

    let _ = visit_expressions_mut(&mut query, |e| {
        // Cannot use a collapsed pattern guard here: `rewrite_type_in_datatype`
        // takes `&mut data_type`, but pattern-guard bindings are immutable (E0596).
        #[allow(clippy::collapsible_match)]
        match e {
            AstExpr::Cast { data_type, .. }
            | AstExpr::TryCast { data_type, .. }
            | AstExpr::SafeCast { data_type, .. }
            | AstExpr::TypedString { data_type, .. } => {
                changed |= rewrite_type_in_datatype(
                    data_type,
                    &old_schema,
                    &old_name,
                    &new_schema,
                    &new_name,
                    allow_unqualified_type_match,
                );
            }
            _ => {}
        }
        ControlFlow::<()>::Continue(())
    });

    if changed {
        Ok(Some(query.to_string()))
    } else {
        Ok(None)
    }
}

pub(super) fn rewrite_expr_type_casts(
    expr_sql: &str,
    old_full: &str,
    new_full: &str,
    allow_unqualified_type_match: bool,
) -> Result<Option<String>> {
    let mut expr = parse_sql_expr(expr_sql)?;
    let (old_schema, old_name) = names::parse_full_name(old_full)?;
    let (new_schema, new_name) = names::parse_full_name(new_full)?;
    let mut changed = false;

    let _ = visit_expressions_mut(&mut expr, |e| {
        // Cannot use a collapsed pattern guard here: `rewrite_type_in_datatype`
        // takes `&mut data_type`, but pattern-guard bindings are immutable (E0596).
        #[allow(clippy::collapsible_match)]
        match e {
            AstExpr::Cast { data_type, .. }
            | AstExpr::TryCast { data_type, .. }
            | AstExpr::SafeCast { data_type, .. }
            | AstExpr::TypedString { data_type, .. } => {
                changed |= rewrite_type_in_datatype(
                    data_type,
                    &old_schema,
                    &old_name,
                    &new_schema,
                    &new_name,
                    allow_unqualified_type_match,
                );
            }
            _ => {}
        }
        ControlFlow::<()>::Continue(())
    });

    if changed {
        Ok(Some(expr.to_string()))
    } else {
        Ok(None)
    }
}

fn rewrite_type_in_datatype(
    data_type: &mut SqlDataType,
    old_schema: &str,
    old_name: &str,
    new_schema: &str,
    new_name: &str,
    allow_unqualified_type_match: bool,
) -> bool {
    match data_type {
        SqlDataType::Custom(type_name, _) => rewrite_custom_type_name(
            type_name,
            old_schema,
            old_name,
            new_schema,
            new_name,
            allow_unqualified_type_match,
        ),
        SqlDataType::Array(elem) => match elem {
            sqlparser::ast::ArrayElemTypeDef::None => false,
            sqlparser::ast::ArrayElemTypeDef::AngleBracket(inner)
            | sqlparser::ast::ArrayElemTypeDef::SquareBracket(inner) => rewrite_type_in_datatype(
                inner,
                old_schema,
                old_name,
                new_schema,
                new_name,
                allow_unqualified_type_match,
            ),
        },
        SqlDataType::Struct(fields) => fields.iter_mut().any(|f| {
            rewrite_type_in_datatype(
                &mut f.field_type,
                old_schema,
                old_name,
                new_schema,
                new_name,
                allow_unqualified_type_match,
            )
        }),
        _ => false,
    }
}

fn rewrite_custom_type_name(
    type_name: &mut ObjectName,
    old_schema: &str,
    old_name: &str,
    new_schema: &str,
    new_name: &str,
    allow_unqualified_type_match: bool,
) -> bool {
    if type_name.0.len() == 1 {
        if allow_unqualified_type_match && normalize_ident(&type_name.0[0]) == old_name {
            assign_ident(&mut type_name.0[0], new_name);
            return true;
        }
        return false;
    }

    if type_name.0.len() >= 2 {
        let schema_idx = type_name.0.len() - 2;
        let name_idx = type_name.0.len() - 1;
        if normalize_ident(&type_name.0[schema_idx]) == old_schema
            && normalize_ident(&type_name.0[name_idx]) == old_name
        {
            assign_ident(&mut type_name.0[schema_idx], new_schema);
            assign_ident(&mut type_name.0[name_idx], new_name);
            return true;
        }
    }

    false
}

fn assign_ident(ident: &mut Ident, new_value: &str) {
    ident.value = new_value.to_string();
    ident.quote_style = required_quote_style(new_value);
}

fn required_quote_style(value: &str) -> Option<char> {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return Some('"');
    };
    let valid_first = first.is_ascii_alphabetic() || first == '_' || first == '$';
    let valid_rest = chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    if valid_first && valid_rest && value == value.to_ascii_lowercase() {
        None
    } else {
        Some('"')
    }
}

pub(super) fn parse_sql_expr(expr_sql: &str) -> Result<AstExpr> {
    let dialect = PostgreSqlDialect {};
    Parser::new(&dialect)
        .try_with_sql(expr_sql)
        .and_then(|mut p| p.parse_expr())
        .map_err(|e| anyhow!("failed to parse expression '{}': {}", expr_sql, e))
}

pub(super) fn parse_stored_query(sql: &str) -> Result<Query> {
    let dialect = PostgreSqlDialect {};
    let stmts = Parser::parse_sql(&dialect, sql)
        .map_err(|e| anyhow!("failed to parse stored query '{}': {}", sql, e))?;
    match stmts.as_slice() {
        [Statement::Query(q)] => Ok(q.as_ref().clone()),
        _ => Err(anyhow!("stored SQL is not a single query")),
    }
}
