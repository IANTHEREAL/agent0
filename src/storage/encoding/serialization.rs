//! Bincode schema/function/row serialization with versioning.
//!
//! Stored values use a magic header prefix to allow future format evolution
//! without breaking older clusters.

use crate::types::{FunctionDef, Row, TableSchema};
use anyhow::{Context, Result};
use std::sync::Once;

const LEGACY_SCHEMA_DESERIALIZATION_SUNSET_DATE: &str = "2026-12-31";

fn warn_legacy_schema_deserialization_once() {
    static WARN_ONCE: Once = Once::new();
    WARN_ONCE.call_once(|| {
        tracing::warn!(
            sunset_date = LEGACY_SCHEMA_DESERIALIZATION_SUNSET_DATE,
            commit = "ce73a8a",
            "legacy schema deserialization fallback is active; remove after all persisted schemas include IndexDef.state"
        );
    });
}

/// Serialize a table schema
pub fn serialize_schema(schema: &TableSchema) -> Result<Vec<u8>> {
    const SCHEMA_MAGIC: &[u8] = b"PGTIKV_SCHEMA_V1\0";
    let payload = bincode::serialize(schema).context("Failed to serialize schema")?;
    let mut out = Vec::with_capacity(SCHEMA_MAGIC.len() + payload.len());
    out.extend_from_slice(SCHEMA_MAGIC);
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Deserialize a table schema.
///
/// Handles backward compatibility: schemas serialized before the `IndexDef.state`
/// field was added (pre-`ce73a8a`) are transparently upgraded by falling back to
/// a legacy struct layout when the primary deserialization fails.
pub fn deserialize_schema(data: &[u8]) -> Result<TableSchema> {
    const SCHEMA_MAGIC: &[u8] = b"PGTIKV_SCHEMA_V1\0";

    let payload = data.strip_prefix(SCHEMA_MAGIC).context(
        "Schema data missing PGTIKV_SCHEMA_V1 header (V1 legacy format no longer supported)",
    )?;

    // Try current format first.
    if let Ok(schema) = bincode::deserialize::<TableSchema>(payload) {
        return Ok(schema);
    }

    // Fallback: deserialize with legacy IndexDef (no `state` field), then upgrade.
    // Sunset policy: remove after 2026-12-31 once all keyspaces are migrated.
    warn_legacy_schema_deserialization_once();
    let legacy: TableSchemaLegacy = bincode::deserialize(payload)
        .context("Failed to deserialize schema (tried both current and legacy formats)")?;
    Ok(legacy.into())
}

/// Legacy IndexDef without the `state` field (added in ce73a8a).
#[derive(serde::Deserialize)]
struct IndexDefLegacy {
    pub name: String,
    pub id: u64,
    pub columns: Vec<String>,
    pub unique: bool,
    pub method: Option<String>,
    pub predicate: Option<String>,
    pub expressions: Vec<String>,
}

/// Legacy TableSchema matching the pre-ce73a8a serialization format.
#[derive(serde::Deserialize)]
struct TableSchemaLegacy {
    pub name: String,
    pub table_id: u64,
    pub columns: Vec<crate::types::ColumnDef>,
    pub version: u64,
    pub pk_constraint_name: Option<String>,
    pub pk_indices: Vec<usize>,
    pub indexes: Vec<IndexDefLegacy>,
    pub check_constraints: Vec<crate::types::CheckConstraint>,
    pub foreign_keys: Vec<crate::types::ForeignKeyConstraint>,
    pub owner: String,
}

impl From<TableSchemaLegacy> for TableSchema {
    fn from(legacy: TableSchemaLegacy) -> Self {
        use crate::types::IndexDef;
        use crate::worker::types::IndexState;

        TableSchema {
            name: legacy.name,
            table_id: legacy.table_id,
            columns: legacy.columns,
            version: legacy.version,
            pk_constraint_name: legacy.pk_constraint_name,
            pk_indices: legacy.pk_indices,
            indexes: legacy
                .indexes
                .into_iter()
                .map(|idx| IndexDef {
                    name: idx.name,
                    id: idx.id,
                    columns: idx.columns,
                    unique: idx.unique,
                    method: idx.method,
                    predicate: idx.predicate,
                    expressions: idx.expressions,
                    state: IndexState::Ready,
                })
                .collect(),
            check_constraints: legacy.check_constraints,
            foreign_keys: legacy.foreign_keys,
            owner: legacy.owner,
            from_alias: None,
        }
    }
}

/// Serialize a row
pub fn serialize_row(row: &Row) -> Result<Vec<u8>> {
    bincode::serialize(row).context("Failed to serialize row")
}

/// Deserialize a row
pub fn deserialize_row(data: &[u8]) -> Result<Row> {
    bincode::deserialize(data).context("Failed to deserialize row")
}

/// Serialize a function definition.
///
/// Stored values are versioned (magic header + bincode payload) to allow future evolution without
/// breaking older clusters.
pub fn serialize_function_def(def: &FunctionDef) -> Result<Vec<u8>> {
    const FUNCTION_MAGIC: &[u8] = b"PGTIKV_FUNCTION_V1\0";
    let payload = bincode::serialize(def).context("Failed to serialize function definition")?;
    let mut out = Vec::with_capacity(FUNCTION_MAGIC.len() + payload.len());
    out.extend_from_slice(FUNCTION_MAGIC);
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Deserialize a function definition.
pub fn deserialize_function_def(data: &[u8]) -> Result<FunctionDef> {
    const FUNCTION_MAGIC: &[u8] = b"PGTIKV_FUNCTION_V1\0";

    let payload = data.strip_prefix(FUNCTION_MAGIC).context(
        "Function data missing PGTIKV_FUNCTION_V1 header (V1 legacy format no longer supported)",
    )?;
    bincode::deserialize(payload).context("Failed to deserialize function definition")
}
