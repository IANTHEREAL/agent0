use super::helpers::{bool_col, int_col, int_val, owner_role_oid, schema_oid, text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema, Value};
use crate::sql::catalog_oids;
use crate::sql::types::registry::{global_registry, ReturnType};
use anyhow::Result;
use async_trait::async_trait;

pub struct PgProc;

#[async_trait]
impl VirtualTable for PgProc {
    fn name(&self) -> &str {
        "pg_proc"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn schema(&self) -> TableSchema {
        TableSchema::virtual_table(
            "pg_proc",
            vec![
                int_col("oid"),
                text_col("proname"),
                int_col("pronamespace"),
                int_col("proowner"),
                int_col("prorettype"),
                text_col("prokind"),
                bool_col("prosecdef"),
            ],
        )
    }

    async fn scan(&self, ctx: &mut ScanContext<'_>) -> Result<Vec<Row>> {
        let pg_catalog_oid = schema_oid(ctx.schema_oids, "pg_catalog");
        let extensions_oid = schema_oid(ctx.schema_oids, crate::extensions::EXTENSIONS_SCHEMA);

        let mut funcs = ctx.store.list_functions(ctx.txn, ctx.db_id).await?;
        funcs.sort_by_key(|f| f.oid);

        let mut rows = Vec::new();

        // HTTP extension functions are registered in global_registry for
        // analysis-time signature validation, but exposed in pg_proc only via
        // the extension-specific path (lines below). Filter them here to
        // prevent duplicate rows.
        let http_names = crate::sql::types::registry::http::HTTP_FUNCTION_NAMES;
        let mut builtin_funcs: Vec<(String, &crate::sql::types::registry::FunctionSignature)> =
            global_registry()
                .iter()
                .filter_map(|(name, sig)| match &sig.return_type {
                    ReturnType::Fixed(_) => {
                        let upper = name.to_ascii_uppercase();
                        if http_names.contains(&upper.as_str()) {
                            None
                        } else {
                            Some((name.to_ascii_lowercase(), sig))
                        }
                    }
                    _ => None,
                })
                .collect();
        builtin_funcs.sort_by(|lhs, rhs| lhs.0.cmp(&rhs.0));

        for (name, sig) in builtin_funcs {
            let ReturnType::Fixed(prorettype) = &sig.return_type else {
                continue;
            };
            let prokind = if sig.is_aggregate {
                "a"
            } else if sig.is_window {
                "w"
            } else {
                "f"
            };
            rows.push(Row::new(vec![
                int_val(catalog_oids::pg_builtin_function_oid(&name)),
                text_val(&name),
                int_val(pg_catalog_oid),
                int_val(catalog_oids::pg_role_oid("postgres")),
                int_val(crate::sql::pg_types::oid_and_typlen_for_datatype(prorettype).0),
                text_val(prokind),
                Value::Boolean(false),
            ]));
        }

        if let Some(ext) = ctx.store.get_extension(ctx.txn, ctx.db_id, "http").await? {
            if ext.enabled {
                for (oid, name) in [
                    (1101_i64, "http_get"),
                    (1102_i64, "http_post"),
                    (1103_i64, "http_put"),
                    (1104_i64, "http_delete"),
                    (1105_i64, "http_head"),
                    (1106_i64, "http"),
                    (1107_i64, "http_patch"),
                ] {
                    rows.push(Row::new(vec![
                        int_val(oid),
                        text_val(name),
                        int_val(extensions_oid),
                        int_val(catalog_oids::pg_role_oid("postgres")),
                        int_val(3802), // jsonb OID — matches scalar return type
                        text_val("f"),
                        Value::Boolean(false),
                    ]));
                }
            }
        }

        if let Some(ext) = ctx
            .store
            .get_extension(ctx.txn, ctx.db_id, "embedding")
            .await?
        {
            if ext.enabled {
                for (oid, name, prorettype) in [
                    (1201_i64, "embedding", crate::sql::pg_types::OID_VECTOR),
                    (1202_i64, "embedding_usage", 2249_i64),
                ] {
                    rows.push(Row::new(vec![
                        int_val(oid),
                        text_val(name),
                        int_val(extensions_oid),
                        int_val(catalog_oids::pg_role_oid("postgres")),
                        int_val(prorettype),
                        text_val("f"),
                        Value::Boolean(false),
                    ]));
                }
            }
        }

        for f in funcs {
            let oid = catalog_oids::pg_proc_function_oid(f.oid);
            let namespace_oid = schema_oid(ctx.schema_oids, &f.schema);
            let ret = f.return_type.to_ascii_lowercase();
            let base_ret = ret.trim().strip_prefix("setof ").unwrap_or(ret.trim());

            let prorettype = if base_ret.contains('.') {
                ctx.store
                    .get_type(ctx.txn, ctx.db_id, base_ret)
                    .await?
                    .map(|t| t.oid as i64)
                    .unwrap_or(25)
            } else {
                match base_ret.split_whitespace().next().unwrap_or(base_ret) {
                    "bool" | "boolean" => 16,
                    "int2" | "smallint" => 21,
                    "int" | "int4" | "integer" => 23,
                    "int8" | "bigint" => 20,
                    "text" => 25,
                    "bytea" => 17,
                    "uuid" => 2950,
                    "date" => 1082,
                    "timestamp" | "timestamptz" => 1114,
                    "time" => 1083,
                    "interval" => 1186,
                    "json" => 114,
                    "jsonb" => 3802,
                    "numeric" | "decimal" => 1700,
                    "trigger" => 2279,
                    _ => 25,
                }
            };

            rows.push(Row::new(vec![
                int_val(oid),
                text_val(&f.name),
                int_val(namespace_oid),
                int_val(owner_role_oid(Some(&f.owner), ctx.current_user)),
                int_val(prorettype),
                text_val("f"),
                Value::Boolean(f.security_definer),
            ]));
        }

        Ok(rows)
    }
}
