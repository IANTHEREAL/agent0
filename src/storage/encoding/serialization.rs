//! Schema/function/row serialization with versioning.
//!
//! ## Schema wire format
//!
//! ```text
//! V2:  b"DB9_SCHEMA_V2\0" ++ MessagePack (named map)  ← opt-in via serialize_schema_v2()
//! V1:  b"DB9_SCHEMA_V1\0" ++ bincode payload          ← default write format
//! ```
//!
//! **Read** supports both V2 and V1 transparently.
//!
//! **Write** defaults to V1 bincode so older binaries can still read.
//! Call `serialize_schema_v2()` to write V2 msgpack — enable once all
//! nodes in the cluster can read V2 (i.e. run this code or newer).
//!
//! V2 uses MessagePack named-map mode so `#[serde(default)]` works
//! natively — future field additions need zero legacy structs.
//!
//! V1 bincode is frozen; three sub-eras are handled for read compat.

use crate::model::{FunctionDef, Row, TableSchema};
use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;

const SCHEMA_MAGIC_V2: &[u8] = b"DB9_SCHEMA_V2\0";
const SCHEMA_MAGIC_V1: &[u8] = b"DB9_SCHEMA_V1\0";
const LEGACY_SUNSET_DATE: &str = "2026-12-31";

static USE_V2_SCHEMA_FORMAT: AtomicBool = AtomicBool::new(false);

pub fn serialize_schema(schema: &TableSchema) -> Result<Vec<u8>> {
    if USE_V2_SCHEMA_FORMAT.load(Ordering::Relaxed) {
        return serialize_schema_v2(schema);
    }
    let payload = bincode::serialize(schema).context("Failed to serialize schema")?;
    let mut out = Vec::with_capacity(SCHEMA_MAGIC_V1.len() + payload.len());
    out.extend_from_slice(SCHEMA_MAGIC_V1);
    out.extend_from_slice(&payload);
    Ok(out)
}

pub fn serialize_schema_v2(schema: &TableSchema) -> Result<Vec<u8>> {
    let payload =
        rmp_serde::to_vec_named(schema).context("Failed to serialize schema to MessagePack")?;
    let mut out = Vec::with_capacity(SCHEMA_MAGIC_V2.len() + payload.len());
    out.extend_from_slice(SCHEMA_MAGIC_V2);
    out.extend_from_slice(&payload);
    Ok(out)
}

pub fn deserialize_schema(data: &[u8]) -> Result<TableSchema> {
    if let Some(payload) = data.strip_prefix(SCHEMA_MAGIC_V2) {
        return rmp_serde::from_slice(payload)
            .context("Failed to deserialize V2 MessagePack schema");
    }
    if let Some(payload) = data.strip_prefix(SCHEMA_MAGIC_V1) {
        return deserialize_v1_bincode(payload);
    }
    anyhow::bail!("Schema data missing magic header (expected DB9_SCHEMA_V2 or V1)")
}

// ===== V1 bincode (frozen — read only) =====
//
// | Era | ColumnDef.collation | IndexDef.state |
// |-----|---------------------|----------------|
// |  3  | ✓                   | ✓              |
// |  2  | ✗                   | ✓              |
// |  1  | ✗                   | ✗              |

fn warn_v1_once(era: u8) {
    static W1: Once = Once::new();
    static W2: Once = Once::new();
    static W3: Once = Once::new();
    let w = match era {
        1 => &W1,
        2 => &W2,
        _ => &W3,
    };
    w.call_once(|| {
        tracing::warn!(
            sunset_date = LEGACY_SUNSET_DATE,
            era,
            "V1 bincode schema active; will be rewritten as V2 msgpack on next DDL"
        );
    });
}

fn deserialize_v1_bincode(payload: &[u8]) -> Result<TableSchema> {
    if let Ok(s) = bincode::deserialize::<TableSchema>(payload) {
        warn_v1_once(3);
        return Ok(s);
    }
    if let Ok(s) = bincode::deserialize::<V1Era2Schema>(payload) {
        warn_v1_once(2);
        return Ok(s.into());
    }
    if let Ok(s) = bincode::deserialize::<V1Era1Schema>(payload) {
        warn_v1_once(1);
        return Ok(s.into());
    }
    anyhow::bail!(
        "Failed to deserialize V1 bincode schema (tried era-3, era-2/pre-collation, era-1/pre-index-state)"
    )
}

// ===== V1 frozen struct mirrors =====

#[derive(serde::Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
struct V1ColumnDef {
    pub name: String,
    pub data_type: crate::model::DataType,
    pub nullable: bool,
    pub primary_key: bool,
    pub unique: bool,
    pub is_serial: bool,
    pub default_expr: Option<String>,
}

impl From<V1ColumnDef> for crate::model::ColumnDef {
    fn from(old: V1ColumnDef) -> Self {
        crate::model::ColumnDef {
            name: old.name,
            data_type: old.data_type,
            nullable: old.nullable,
            primary_key: old.primary_key,
            unique: old.unique,
            is_serial: old.is_serial,
            default_expr: old.default_expr,
            collation: None,
        }
    }
}

#[derive(serde::Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
struct V1IndexDef {
    pub name: String,
    pub id: u64,
    pub columns: Vec<String>,
    pub unique: bool,
    pub method: Option<String>,
    pub predicate: Option<String>,
    pub expressions: Vec<String>,
}

// Era 2: has IndexDef.state, no ColumnDef.collation
#[derive(serde::Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
struct V1Era2Schema {
    pub name: String,
    pub table_id: u64,
    pub columns: Vec<V1ColumnDef>,
    pub version: u64,
    pub pk_constraint_name: Option<String>,
    pub pk_indices: Vec<usize>,
    pub indexes: Vec<crate::model::IndexDef>,
    pub check_constraints: Vec<crate::model::CheckConstraint>,
    pub foreign_keys: Vec<crate::model::ForeignKeyConstraint>,
    pub owner: String,
}

impl From<V1Era2Schema> for TableSchema {
    fn from(s: V1Era2Schema) -> Self {
        TableSchema {
            name: s.name,
            table_id: s.table_id,
            columns: s.columns.into_iter().map(Into::into).collect(),
            version: s.version,
            pk_constraint_name: s.pk_constraint_name,
            pk_indices: s.pk_indices,
            indexes: s.indexes,
            check_constraints: s.check_constraints,
            foreign_keys: s.foreign_keys,
            owner: s.owner,
            from_alias: None,
        }
    }
}

// Era 1: no IndexDef.state, no ColumnDef.collation
#[derive(serde::Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
struct V1Era1Schema {
    pub name: String,
    pub table_id: u64,
    pub columns: Vec<V1ColumnDef>,
    pub version: u64,
    pub pk_constraint_name: Option<String>,
    pub pk_indices: Vec<usize>,
    pub indexes: Vec<V1IndexDef>,
    pub check_constraints: Vec<crate::model::CheckConstraint>,
    pub foreign_keys: Vec<crate::model::ForeignKeyConstraint>,
    pub owner: String,
}

impl From<V1Era1Schema> for TableSchema {
    fn from(s: V1Era1Schema) -> Self {
        use crate::model::IndexDef;
        use crate::worker::types::IndexState;

        TableSchema {
            name: s.name,
            table_id: s.table_id,
            columns: s.columns.into_iter().map(Into::into).collect(),
            version: s.version,
            pk_constraint_name: s.pk_constraint_name,
            pk_indices: s.pk_indices,
            indexes: s
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
            check_constraints: s.check_constraints,
            foreign_keys: s.foreign_keys,
            owner: s.owner,
            from_alias: None,
        }
    }
}

pub fn serialize_row(row: &Row) -> Result<Vec<u8>> {
    bincode::serialize(row).context("Failed to serialize row")
}

pub fn deserialize_row(data: &[u8]) -> Result<Row> {
    bincode::deserialize(data).context("Failed to deserialize row")
}

pub fn serialize_function_def(def: &FunctionDef) -> Result<Vec<u8>> {
    const FUNCTION_MAGIC: &[u8] = b"DB9_FUNCTION_V1\0";
    let payload = bincode::serialize(def).context("Failed to serialize function definition")?;
    let mut out = Vec::with_capacity(FUNCTION_MAGIC.len() + payload.len());
    out.extend_from_slice(FUNCTION_MAGIC);
    out.extend_from_slice(&payload);
    Ok(out)
}

pub fn deserialize_function_def(data: &[u8]) -> Result<FunctionDef> {
    const FUNCTION_MAGIC: &[u8] = b"DB9_FUNCTION_V1\0";
    let payload = data.strip_prefix(FUNCTION_MAGIC).context(
        "Function data missing DB9_FUNCTION_V1 header (V1 legacy format no longer supported)",
    )?;
    bincode::deserialize(payload).context("Failed to deserialize function definition")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ColumnDef, DataType, IndexDef};
    use crate::worker::types::IndexState;

    fn sample_schema() -> TableSchema {
        TableSchema {
            name: "public.users".into(),
            table_id: 42,
            columns: vec![
                ColumnDef {
                    name: "id".into(),
                    data_type: DataType::Int64,
                    nullable: false,
                    primary_key: true,
                    unique: true,
                    is_serial: true,
                    default_expr: None,
                    collation: None,
                },
                ColumnDef {
                    name: "name".into(),
                    data_type: DataType::Text,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    collation: Some("en_US".into()),
                },
            ],
            version: 1,
            pk_constraint_name: Some("users_pkey".into()),
            pk_indices: vec![0],
            indexes: vec![IndexDef {
                name: "users_name_idx".into(),
                id: 1,
                columns: vec!["name".into()],
                unique: false,
                method: Some("btree".into()),
                predicate: None,
                expressions: vec![],
                state: IndexState::Ready,
            }],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: "admin".into(),
            from_alias: None,
        }
    }

    #[test]
    fn default_write_is_v1_bincode() {
        USE_V2_SCHEMA_FORMAT.store(false, Ordering::Relaxed);
        let schema = sample_schema();
        let data = serialize_schema(&schema).unwrap();
        assert!(data.starts_with(SCHEMA_MAGIC_V1));
        let decoded = deserialize_schema(&data).unwrap();
        assert_eq!(decoded.name, schema.name);
        assert_eq!(decoded.columns[1].collation, Some("en_US".into()));
    }

    #[test]
    fn v2_msgpack_round_trip() {
        let schema = sample_schema();
        let data = serialize_schema_v2(&schema).unwrap();
        assert!(data.starts_with(SCHEMA_MAGIC_V2));
        let decoded = deserialize_schema(&data).unwrap();
        assert_eq!(decoded.name, schema.name);
        assert_eq!(decoded.table_id, schema.table_id);
        assert_eq!(decoded.columns.len(), 2);
        assert_eq!(decoded.columns[1].collation, Some("en_US".into()));
        assert_eq!(decoded.indexes[0].state, IndexState::Ready);
    }

    #[test]
    fn v1_bincode_era3_compat() {
        let schema = sample_schema();
        let payload = bincode::serialize(&schema).unwrap();
        let mut data = Vec::from(SCHEMA_MAGIC_V1);
        data.extend_from_slice(&payload);
        let decoded = deserialize_schema(&data).unwrap();
        assert_eq!(decoded.name, "public.users");
        assert_eq!(decoded.columns[1].collation, Some("en_US".into()));
    }

    #[test]
    fn v1_bincode_era2_compat() {
        let era2 = V1Era2Schema {
            name: "public.old_table".into(),
            table_id: 7,
            columns: vec![V1ColumnDef {
                name: "col1".into(),
                data_type: DataType::Text,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            }],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: "admin".into(),
        };
        let payload = bincode::serialize(&era2).unwrap();
        let mut data = Vec::from(SCHEMA_MAGIC_V1);
        data.extend_from_slice(&payload);
        let decoded = deserialize_schema(&data).unwrap();
        assert_eq!(decoded.name, "public.old_table");
        assert_eq!(decoded.columns[0].collation, None);
    }

    #[test]
    fn v1_bincode_era1_compat() {
        let era1 = V1Era1Schema {
            name: "public.ancient".into(),
            table_id: 3,
            columns: vec![V1ColumnDef {
                name: "x".into(),
                data_type: DataType::Int32,
                nullable: false,
                primary_key: true,
                unique: true,
                is_serial: false,
                default_expr: None,
            }],
            version: 1,
            pk_constraint_name: Some("ancient_pkey".into()),
            pk_indices: vec![0],
            indexes: vec![V1IndexDef {
                name: "ancient_idx".into(),
                id: 1,
                columns: vec!["x".into()],
                unique: true,
                method: None,
                predicate: None,
                expressions: vec![],
            }],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: "admin".into(),
        };
        let payload = bincode::serialize(&era1).unwrap();
        let mut data = Vec::from(SCHEMA_MAGIC_V1);
        data.extend_from_slice(&payload);
        let decoded = deserialize_schema(&data).unwrap();
        assert_eq!(decoded.name, "public.ancient");
        assert_eq!(decoded.indexes[0].state, IndexState::Ready);
        assert_eq!(decoded.columns[0].collation, None);
    }

    #[test]
    fn v2_tolerates_unknown_fields() {
        // Simulate a future binary that added a field — msgpack named-map ignores unknowns.
        let schema = sample_schema();
        let json = serde_json::to_value(&schema).unwrap();
        let mut map: serde_json::Map<String, serde_json::Value> = json.as_object().unwrap().clone();
        map.insert(
            "future_field".into(),
            serde_json::Value::String("hello".into()),
        );
        // Re-encode via msgpack named map
        let payload = rmp_serde::to_vec_named(&map).unwrap();
        let mut data = Vec::from(SCHEMA_MAGIC_V2);
        data.extend_from_slice(&payload);
        let decoded = deserialize_schema(&data).unwrap();
        assert_eq!(decoded.name, "public.users");
    }

    #[test]
    fn v2_tolerates_missing_default_fields() {
        let schema = sample_schema();
        let json = serde_json::to_value(&schema).unwrap();
        let mut map: serde_json::Map<String, serde_json::Value> = json.as_object().unwrap().clone();
        map.remove("check_constraints");
        let payload = rmp_serde::to_vec_named(&map).unwrap();
        let mut data = Vec::from(SCHEMA_MAGIC_V2);
        data.extend_from_slice(&payload);
        let decoded = deserialize_schema(&data).unwrap();
        assert!(decoded.check_constraints.is_empty());
    }

    #[test]
    fn bad_magic_rejected() {
        assert!(deserialize_schema(b"GARBAGE\0payload").is_err());
    }
}
