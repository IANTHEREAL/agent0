use super::*;

pub(super) fn find_text_column<'a>(
    schema: Option<&'a TableSchema>,
    primary: &str,
) -> Option<(&'a TableSchema, usize)> {
    let schema = schema?;
    if let Some(idx) = schema
        .columns
        .iter()
        .position(|c| c.name.eq_ignore_ascii_case(primary))
    {
        return Some((schema, idx));
    }
    None
}

pub(super) fn value_to_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Int32(n) => Some(*n as i64),
        Value::Int64(n) => Some(*n),
        Value::Float64(n) => Some(*n as i64),
        Value::Text(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

pub(super) fn value_to_i64_strict(v: &Value, arg_name: &str) -> Result<i64> {
    match v {
        Value::Int32(n) => Ok(*n as i64),
        Value::Int64(n) => Ok(*n),
        Value::Text(s) => s.trim().parse::<i64>().map_err(|_| {
            anyhow!(
                "pg_get_indexdef(oid, column_no, pretty): {} must be integer-compatible, got {:?}",
                arg_name,
                v
            )
        }),
        _ => Err(anyhow!(
            "pg_get_indexdef(oid, column_no, pretty): {} must be integer-compatible, got {:?}",
            arg_name,
            v
        )),
    }
}

pub(super) fn value_to_bool_strict(v: &Value, arg_name: &str) -> Result<bool> {
    match v {
        Value::Boolean(b) => Ok(*b),
        _ => Err(anyhow!(
            "pg_get_indexdef(oid, column_no, pretty): {} must be boolean, got {:?}",
            arg_name,
            v
        )),
    }
}

pub(super) async fn lookup_indexdef_by_oid(
    store: &Arc<crate::storage::TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    oid: i64,
) -> Result<Option<String>> {
    use crate::sql::catalog::helpers::{format_indexdef, split_schema_and_name};
    use crate::sql::catalog_oids;

    let user_tables = store.list_tables(txn, db_id).await?;

    for full_table_name in user_tables {
        let (table_schema, table_name) = split_schema_and_name(&full_table_name);
        let Some(schema) = store.get_schema(txn, db_id, &full_table_name).await? else {
            continue;
        };

        for idx in &schema.indexes {
            let index_oid = catalog_oids::pg_class_index_oid(schema.table_id, idx.id)?;
            if index_oid == oid {
                return Ok(Some(format_indexdef(&table_schema, &table_name, idx)));
            }
        }

        if !schema.pk_indices.is_empty() {
            let pk_oid = catalog_oids::pg_class_pk_index_oid(schema.table_id)?;
            if pk_oid == oid {
                let pk_cols: Vec<String> = schema
                    .pk_indices
                    .iter()
                    .filter_map(|idx| schema.columns.get(*idx).map(|c| c.name.clone()))
                    .collect();
                let indexdef = format!(
                    "CREATE UNIQUE INDEX {}_pkey ON {}.{} USING btree ({})",
                    table_name,
                    table_schema,
                    table_name,
                    pk_cols.join(", ")
                );
                return Ok(Some(indexdef));
            }
        }
    }

    Ok(None)
}

pub(super) async fn lookup_index_column_by_oid(
    store: &Arc<crate::storage::TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    oid: i64,
    col_no: usize,
) -> Result<Option<Option<String>>> {
    use crate::sql::catalog_oids;

    let user_tables = store.list_tables(txn, db_id).await?;

    for full_table_name in user_tables {
        let Some(schema) = store.get_schema(txn, db_id, &full_table_name).await? else {
            continue;
        };

        for idx in &schema.indexes {
            let index_oid = catalog_oids::pg_class_index_oid(schema.table_id, idx.id)?;
            if index_oid == oid {
                let mut elements: Vec<String> = idx.columns.clone();
                elements.extend(idx.expressions.iter().cloned());
                let element = elements.get(col_no - 1).cloned();
                return Ok(Some(element));
            }
        }

        if !schema.pk_indices.is_empty() {
            let pk_oid = catalog_oids::pg_class_pk_index_oid(schema.table_id)?;
            if pk_oid == oid {
                let pk_cols: Vec<String> = schema
                    .pk_indices
                    .iter()
                    .filter_map(|i| schema.columns.get(*i).map(|c| c.name.clone()))
                    .collect();
                let element = pk_cols.get(col_no - 1).cloned();
                return Ok(Some(element));
            }
        }
    }

    Ok(None)
}

pub(super) async fn index_oid_exists_by_oid(
    store: &Arc<crate::storage::TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    oid: i64,
) -> Result<bool> {
    use crate::sql::catalog_oids;

    let user_tables = store.list_tables(txn, db_id).await?;

    for full_table_name in user_tables {
        let Some(schema) = store.get_schema(txn, db_id, &full_table_name).await? else {
            continue;
        };

        for idx in &schema.indexes {
            let index_oid = catalog_oids::pg_class_index_oid(schema.table_id, idx.id)?;
            if index_oid == oid {
                return Ok(true);
            }
        }

        if !schema.pk_indices.is_empty() {
            let pk_oid = catalog_oids::pg_class_pk_index_oid(schema.table_id)?;
            if pk_oid == oid {
                return Ok(true);
            }
        }
    }

    Ok(false)
}

pub(super) async fn lookup_typname_by_oid(
    store: &Arc<crate::storage::TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    oid: i64,
) -> Result<Option<String>> {
    if let Some(t) = crate::sql::pg_types::format_type_name_for_oid(oid) {
        return Ok(Some(t.to_string()));
    }

    let mut types = store.list_types(txn, db_id).await?;
    types.sort_by_key(|t| t.oid);
    Ok(types
        .into_iter()
        .find(|t| t.oid as i64 == oid)
        .map(|t| t.name))
}

pub(super) async fn lookup_regtype_text_by_oid(
    store: &crate::storage::TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    oid: i64,
    search_path: &[String],
) -> Result<Option<String>> {
    if let Some(name) = crate::sql::pg_types::regtype_text_for_oid(oid) {
        return Ok(Some(name.to_string()));
    }

    let mut types = store.list_types(txn, db_id).await?;
    types.sort_by_key(|t| t.oid);
    let Some(udt) = types.into_iter().find(|t| t.oid as i64 == oid) else {
        return Ok(None);
    };

    format_visible_regtype_name(store, txn, db_id, &udt.schema, &udt.name, search_path)
        .await
        .map(Some)
}

pub(super) fn format_visible_name(schema: &str, name: &str, visible_unqualified: bool) -> String {
    if visible_unqualified {
        crate::sql::quoting::quote_ident(name)
    } else {
        format!(
            "{}.{}",
            crate::sql::quoting::quote_ident(schema),
            crate::sql::quoting::quote_ident(name)
        )
    }
}

pub(super) async fn format_visible_regclass_relname(
    store: &crate::storage::TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    schema: &str,
    name: &str,
    oid: i64,
    search_path: &[String],
) -> Result<String> {
    let visible_unqualified = crate::sql::names::resolve_existing_relation_oid(
        store,
        txn,
        db_id,
        None,
        name,
        search_path,
    )
    .await?
    .is_some_and(|resolved_oid| resolved_oid == oid);

    Ok(format_visible_name(schema, name, visible_unqualified))
}

pub(super) async fn format_visible_regtype_name(
    store: &crate::storage::TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    schema: &str,
    name: &str,
    search_path: &[String],
) -> Result<String> {
    let visible_unqualified = crate::sql::names::resolve_visible_type_full_name(
        store,
        txn,
        db_id,
        None,
        name,
        search_path,
    )
    .await?
    .is_some_and(|resolved| {
        let resolved = resolved.resolved_name();
        resolved.schema == schema && resolved.name == name
    });

    Ok(format_visible_name(schema, name, visible_unqualified))
}

pub(super) async fn lookup_relname_by_regclass_oid(
    store: &crate::storage::TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    oid: i64,
    search_path: &[String],
) -> Result<Option<String>> {
    if let Some((schema, name)) = crate::sql::catalog::catalog_relation_name(oid) {
        return format_visible_regclass_relname(
            store,
            txn,
            db_id,
            &schema,
            &name,
            oid,
            search_path,
        )
        .await
        .map(Some);
    }

    let all_tables = store.list_tables(txn, db_id).await?;
    for table_full in &all_tables {
        let Some(table_schema) = store.get_schema(txn, db_id, table_full).await? else {
            continue;
        };
        let (schema, table_name) =
            crate::sql::names::parse_full_name(table_full).unwrap_or_default();
        if crate::sql::catalog_oids::pg_class_table_oid(table_schema.table_id)? == oid {
            return format_visible_regclass_relname(
                store,
                txn,
                db_id,
                &schema,
                &table_name,
                oid,
                search_path,
            )
            .await
            .map(Some);
        }
        for idx in &table_schema.indexes {
            if crate::sql::catalog_oids::pg_class_index_oid(table_schema.table_id, idx.id)? == oid {
                return format_visible_regclass_relname(
                    store,
                    txn,
                    db_id,
                    &schema,
                    &idx.name,
                    oid,
                    search_path,
                )
                .await
                .map(Some);
            }
        }
        if !table_schema.pk_indices.is_empty()
            && crate::sql::catalog_oids::pg_class_pk_index_oid(table_schema.table_id)? == oid
        {
            let pk_name = table_schema
                .pk_constraint_name
                .clone()
                .unwrap_or_else(|| format!("{}_pkey", table_name));
            return format_visible_regclass_relname(
                store,
                txn,
                db_id,
                &schema,
                &pk_name,
                oid,
                search_path,
            )
            .await
            .map(Some);
        }
    }

    for view_def in store.list_views(txn, db_id).await? {
        if crate::sql::catalog_oids::pg_class_view_oid(view_def.oid) == oid {
            return format_visible_regclass_relname(
                store,
                txn,
                db_id,
                &view_def.schema,
                &view_def.name,
                oid,
                search_path,
            )
            .await
            .map(Some);
        }
    }

    for seq_def in store.list_sequences(txn, db_id).await? {
        if crate::sql::catalog_oids::pg_class_sequence_oid(seq_def.oid) == oid {
            return format_visible_regclass_relname(
                store,
                txn,
                db_id,
                &seq_def.schema,
                &seq_def.name,
                oid,
                search_path,
            )
            .await
            .map(Some);
        }
    }

    Ok(None)
}

pub(super) fn regtype_search_path_schemas(search_path: &[String]) -> Vec<String> {
    crate::sql::names::type_search_path_schemas(search_path)
}

pub(super) fn hstore_extension_oid_for_name(name: &str, is_array: bool) -> Option<i64> {
    let base_oid = if name == "hstore" {
        Some(crate::sql::pg_types::OID_HSTORE)
    } else if name == "_hstore" {
        Some(crate::sql::pg_types::OID_HSTORE_ARRAY)
    } else {
        None
    };

    if is_array {
        match base_oid {
            Some(crate::sql::pg_types::OID_HSTORE) => Some(crate::sql::pg_types::OID_HSTORE_ARRAY),
            _ => None,
        }
    } else {
        base_oid
    }
}

async fn lookup_hstore_extension_oid_if_enabled(
    store: &Arc<crate::storage::TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    name: &str,
    is_array: bool,
) -> Result<Option<i64>> {
    let Some(ext_oid) = hstore_extension_oid_for_name(name, is_array) else {
        return Ok(None);
    };
    let Some(installed) = store.get_extension(txn, db_id, "hstore").await? else {
        return Ok(None);
    };
    if !installed.enabled {
        return Ok(None);
    }
    Ok(Some(ext_oid))
}

#[derive(Clone, Copy)]
pub(super) struct ResolvedRegtypeOid {
    pub(super) oid: i64,
    pub(super) is_user_defined: bool,
}

fn builtin_regtype_oid_in_schema(
    schema: &str,
    name: &str,
    is_array: bool,
) -> Option<ResolvedRegtypeOid> {
    if schema != "pg_catalog" {
        return None;
    }

    let base_oid = crate::sql::pg_types::actual_pg_catalog_regtype_oid(name)?;
    let oid = if is_array {
        crate::sql::pg_types::regtype_array_oid(base_oid)?
    } else {
        base_oid
    };
    Some(ResolvedRegtypeOid {
        oid,
        is_user_defined: false,
    })
}

async fn resolve_regtype_in_schema(
    store: &Arc<crate::storage::TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    schema: &str,
    name: &str,
    is_array: bool,
) -> Result<Option<ResolvedRegtypeOid>> {
    if let Some(builtin) = builtin_regtype_oid_in_schema(schema, name, is_array) {
        return Ok(Some(builtin));
    }

    let full_name = format!("{}.{}", schema, name);
    let Some(def) = store.get_type(txn, db_id, &full_name).await? else {
        return Ok(None);
    };

    if is_array {
        return Ok(None);
    }

    Ok(Some(ResolvedRegtypeOid {
        oid: def.oid as i64,
        is_user_defined: true,
    }))
}

pub(super) async fn lookup_regtype_oid_with_hstore_extension(
    store: &Arc<crate::storage::TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    lookup: &crate::sql::expr::functions::pg_compat::ParsedRegtypeLookup,
    search_path: &[String],
) -> Result<Option<ResolvedRegtypeOid>> {
    if lookup.kind
        == crate::sql::expr::functions::pg_compat::ParsedRegtypeLookupKind::SpecialBuiltin
    {
        return Ok(
            crate::sql::expr::functions::pg_compat::resolve_builtin_regtype_lookup(lookup).map(
                |oid| ResolvedRegtypeOid {
                    oid,
                    is_user_defined: false,
                },
            ),
        );
    }

    let schemas: Vec<String> = match lookup.schema.as_deref() {
        Some(schema) => vec![schema.to_string()],
        None => regtype_search_path_schemas(search_path),
    };

    for schema in schemas {
        if let Some(resolved) =
            resolve_regtype_in_schema(store, txn, db_id, &schema, &lookup.name, lookup.is_array)
                .await?
        {
            return Ok(Some(resolved));
        }

        if schema == "public" {
            if let Some(ext_oid) = lookup_hstore_extension_oid_if_enabled(
                store,
                txn,
                db_id,
                &lookup.name,
                lookup.is_array,
            )
            .await?
            {
                return Ok(Some(ResolvedRegtypeOid {
                    oid: ext_oid,
                    is_user_defined: false,
                }));
            }
        }
    }

    Ok(None)
}
