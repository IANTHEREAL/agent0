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
        // Resolve an overload's Fixed return type (the only shape that
        // can appear in pg_proc — SameAsArg / Custom / FirstNonNull
        // depend on runtime argument types and don't describe a concrete
        // row). Polymorphic functions register one overload per concrete
        // (arg_types, return_type) pair so each appears here, matching
        // PG's pg_proc layout (e.g. SIGN registers two overloads and
        // yields two rows: one for double precision, one for numeric).
        fn fixed_return_type(
            sig: &crate::sql::types::registry::FunctionSignature,
        ) -> Option<crate::model::DataType> {
            match &sig.return_type {
                ReturnType::Fixed(dt) => Some(dt.clone()),
                _ => None,
            }
        }
        let mut builtin_funcs: Vec<(
            String,
            &crate::sql::types::registry::FunctionSignature,
            crate::model::DataType,
        )> = global_registry()
            .iter_overloads()
            .filter_map(|(name, sig)| {
                let upper = name.to_ascii_uppercase();
                if http_names.contains(&upper.as_str()) {
                    return None;
                }
                let prorettype = fixed_return_type(sig)?;
                Some((name.to_ascii_lowercase(), sig, prorettype))
            })
            .collect();
        // Sort by (name, prorettype-OID) so multi-overload functions
        // like SIGN produce a deterministic row order within a name.
        builtin_funcs.sort_by(|lhs, rhs| {
            lhs.0.cmp(&rhs.0).then_with(|| {
                crate::sql::pg_types::oid_and_typlen_for_datatype(&lhs.2)
                    .0
                    .cmp(&crate::sql::pg_types::oid_and_typlen_for_datatype(&rhs.2).0)
            })
        });

        for (name, sig, prorettype) in builtin_funcs {
            let prokind = if sig.is_aggregate {
                "a"
            } else if sig.is_window {
                "w"
            } else {
                "f"
            };
            let prorettype_oid = crate::sql::pg_types::oid_and_typlen_for_datatype(&prorettype).0;
            // Overload-aware OID derivation keyed on (proname,
            // proargtypes) — matches PG's uniqueness key for pg_proc
            // and gives SIGN's dp / numeric rows (and ABS's six
            // numeric-family rows) distinct oids.
            let arg_oids: Vec<i64> = sig
                .arg_types
                .iter()
                .map(|t| crate::sql::pg_types::oid_and_typlen_for_datatype(t).0)
                .collect();
            let oid = catalog_oids::pg_builtin_function_overload_oid(&name, &arg_oids);
            rows.push(Row::new(vec![
                int_val(oid),
                text_val(&name),
                int_val(pg_catalog_oid),
                int_val(catalog_oids::pg_role_oid("postgres")),
                int_val(prorettype_oid),
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
