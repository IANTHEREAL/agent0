use super::helpers::{
    bool_col, int_col, null_val, text_array_col, text_col, text_val, RELKIND_VIEW,
};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema, Value};
use crate::sql::query_context::QueryContext;
use crate::sql::session::settings::GUC_TABLE;
use anyhow::Result;
use async_trait::async_trait;

pub struct PgSettings;

#[async_trait]
impl VirtualTable for PgSettings {
    fn name(&self) -> &str {
        "pg_settings"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn relkind(&self) -> &str {
        RELKIND_VIEW
    }

    fn schema(&self) -> TableSchema {
        TableSchema::virtual_table(
            "pg_settings",
            vec![
                text_col("name"),
                text_col("setting"),
                text_col("unit"),
                text_col("category"),
                text_col("short_desc"),
                text_col("extra_desc"),
                text_col("context"),
                text_col("vartype"),
                text_col("source"),
                text_col("min_val"),
                text_col("max_val"),
                text_array_col("enumvals"),
                text_col("boot_val"),
                text_col("reset_val"),
                text_col("sourcefile"),
                int_col("sourceline"),
                bool_col("pending_restart"),
            ],
        )
    }

    async fn scan(&self, _ctx: &mut ScanContext<'_>) -> Result<Vec<Row>> {
        let mut rows = Vec::with_capacity(GUC_TABLE.len());

        for guc in GUC_TABLE {
            // Try to read the current session value from the settings snapshot;
            // fall back to boot default when no snapshot is available (e.g. in
            // background workers).
            let current_value = QueryContext::current_setting_snapshot(guc.name)
                .unwrap_or_else(|| guc.boot_default.to_string());

            let context_str = match guc.context {
                crate::sql::session::settings::GucContext::Internal => "internal",
                crate::sql::session::settings::GucContext::Suset => "superuser",
                crate::sql::session::settings::GucContext::Userset => "user",
            };

            let vartype_str = match guc.guc_type {
                crate::sql::session::settings::GucType::Bool => "bool",
                crate::sql::session::settings::GucType::Int => "integer",
                crate::sql::session::settings::GucType::Real => "real",
                crate::sql::session::settings::GucType::String => "string",
                crate::sql::session::settings::GucType::Enum => "enum",
                crate::sql::session::settings::GucType::Timeout => "integer",
                crate::sql::session::settings::GucType::ByteSize => "integer",
            };

            // PostgreSQL separates the numeric value (setting) from its
            // unit.  SHOW-formatted values may include a suffix like "ms";
            // strip it so `setting` is the raw number and `unit` carries
            // the base unit string.
            let (setting_display, unit_display): (String, Option<&str>) = match guc.guc_type {
                crate::sql::session::settings::GucType::Timeout => {
                    let raw = current_value.strip_suffix("ms").unwrap_or(&current_value);
                    (raw.to_string(), Some("ms"))
                }
                crate::sql::session::settings::GucType::ByteSize => {
                    (current_value.clone(), Some("B"))
                }
                _ => (current_value.clone(), None),
            };

            // Use tenant-configured reset default when available (e.g.
            // statement_timeout, idle_in_transaction_session_timeout may
            // have per-tenant defaults that differ from the boot default).
            let reset_val = QueryContext::reset_default_from_snapshot(guc.name)
                .unwrap_or_else(|| guc.boot_default.to_string());

            // Compare against the effective reset default, not the boot
            // default, so tenant-configured values show "default" correctly.
            let source = if current_value == reset_val {
                "default"
            } else {
                "session"
            };

            rows.push(Row::new(vec![
                text_val(guc.name),         // name
                text_val(&setting_display), // setting
                match unit_display {
                    // unit
                    Some(u) => text_val(u),
                    None => null_val(),
                },
                null_val(), // category
                if guc.description.is_empty() {
                    // short_desc
                    null_val()
                } else {
                    text_val(guc.description)
                },
                null_val(),                 // extra_desc
                text_val(context_str),      // context
                text_val(vartype_str),      // vartype
                text_val(source),           // source
                null_val(),                 // min_val
                null_val(),                 // max_val
                null_val(),                 // enumvals
                text_val(guc.boot_default), // boot_val
                text_val(&reset_val),       // reset_val
                null_val(),                 // sourcefile
                null_val(),                 // sourceline
                Value::Boolean(false),      // pending_restart
            ]));
        }

        Ok(rows)
    }
}
