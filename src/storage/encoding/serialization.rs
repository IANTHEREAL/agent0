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
use dashmap::DashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Once};

const SCHEMA_MAGIC_V2: &[u8] = b"DB9_SCHEMA_V2\0";
const SCHEMA_MAGIC_V1: &[u8] = b"DB9_SCHEMA_V1\0";
const LEGACY_SUNSET_DATE: &str = "2026-12-31";

static USE_V2_SCHEMA_FORMAT: AtomicBool = AtomicBool::new(false);

/// Process-global cache: maps raw schema bytes → deserialized+hydrated TableSchema.
///
/// Avoids repeated SQL parsing in `hydrate_runtime_caches()` when the same
/// schema bytes are read from TiKV across queries. The cache is content-addressed
/// (keyed by raw bytes), so DDL mutations that produce different serialized bytes
/// automatically miss and re-populate.
///
/// Bounded to MAX_SCHEMA_CACHE_ENTRIES; cleared entirely on overflow (rare —
/// entry count equals the number of distinct table schema blobs ever seen).
const MAX_SCHEMA_CACHE_ENTRIES: usize = 4096;

static SCHEMA_DESER_CACHE: LazyLock<DashMap<Vec<u8>, TableSchema>> = LazyLock::new(DashMap::new);

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
    // Fast path: return cached schema if we've deserialized these exact bytes before.
    // This avoids the SQL parser cost in hydrate_runtime_caches() on repeated reads
    // of the same schema across queries (get_schema() re-reads from TiKV each call).
    if let Some(cached) = SCHEMA_DESER_CACHE.get(data) {
        return Ok(cached.clone());
    }

    let mut schema = if let Some(payload) = data.strip_prefix(SCHEMA_MAGIC_V2) {
        deserialize_v2_msgpack(payload)?
    } else if let Some(payload) = data.strip_prefix(SCHEMA_MAGIC_V1) {
        deserialize_v1_bincode(payload)?
    } else {
        anyhow::bail!("Schema data missing magic header (expected DB9_SCHEMA_V2 or V1)")
    };
    schema.hydrate_runtime_caches();

    // Populate cache. Clear if too large to bound memory usage.
    if SCHEMA_DESER_CACHE.len() >= MAX_SCHEMA_CACHE_ENTRIES {
        SCHEMA_DESER_CACHE.clear();
    }
    SCHEMA_DESER_CACHE.insert(data.to_vec(), schema.clone());

    Ok(schema)
}

#[derive(serde::Deserialize)]
struct V2Schema {
    pub name: String,
    pub table_id: u64,
    pub columns: Vec<crate::model::ColumnDef>,
    pub version: u64,
    #[serde(default)]
    pub pk_constraint_name: Option<String>,
    pub pk_indices: Vec<usize>,
    pub indexes: Vec<V2IndexDef>,
    #[serde(default)]
    pub check_constraints: Vec<crate::model::CheckConstraint>,
    #[serde(default)]
    pub foreign_keys: Vec<crate::model::ForeignKeyConstraint>,
    #[serde(default = "default_v2_owner")]
    pub owner: String,
    #[serde(default)]
    pub from_alias: Option<String>,
}

#[derive(serde::Deserialize)]
struct V2IndexDef {
    pub name: String,
    pub id: u64,
    pub columns: Vec<String>,
    pub unique: bool,
    #[serde(default)]
    pub is_constraint: Option<bool>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub predicate: Option<String>,
    #[serde(default)]
    pub expressions: Vec<String>,
    #[serde(default)]
    pub state: crate::worker::types::IndexState,
}

impl From<V2Schema> for TableSchema {
    fn from(s: V2Schema) -> Self {
        let V2Schema {
            name,
            table_id,
            columns,
            version,
            pk_constraint_name,
            pk_indices,
            indexes,
            check_constraints,
            foreign_keys,
            owner,
            from_alias,
        } = s;

        let decoded_indexes: Vec<crate::model::IndexDef> = indexes
            .into_iter()
            .map(|idx| {
                let is_constraint = idx
                    .is_constraint
                    .unwrap_or_else(|| legacy_index_constraint_default(idx.unique, &idx.columns));
                crate::model::IndexDef {
                    name: idx.name,
                    id: idx.id,
                    columns: idx.columns,
                    unique: idx.unique,
                    is_constraint,
                    method: idx.method,
                    predicate: idx.predicate,
                    expressions: idx.expressions,
                    state: idx.state,
                    cached_predicate_conjuncts: None,
                }
            })
            .collect();

        TableSchema {
            name,
            table_id,
            columns,
            version,
            pk_constraint_name,
            pk_indices,
            indexes: decoded_indexes,
            check_constraints,
            foreign_keys,
            owner,
            from_alias,
        }
    }
}

fn deserialize_v2_msgpack(payload: &[u8]) -> Result<TableSchema> {
    let decoded: V2Schema =
        rmp_serde::from_slice(payload).context("Failed to deserialize V2 MessagePack schema")?;
    Ok(decoded.into())
}

fn default_v2_owner() -> String {
    "postgres".to_string()
}

// ===== V1 bincode (frozen — read only) =====
//
// | Era | ColumnDef.collation | IndexDef.state | IndexDef.is_constraint |
// |-----|---------------------|----------------|------------------------|
// |  4  | ✓                   | ✓              | ✓                      |
// |  3  | ✓                   | ✓              | ✗ (inferred)           |
// |  2  | ✗                   | ✓              | ✗ (inferred)           |
// |  1  | ✗                   | ✗              | ✗ (inferred)           |

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
            "V1 bincode schema active; will be rewritten as V2 msgpack on next DDL (legacy unique indexes default to UNIQUE constraints when legacy metadata is ambiguous)"
        );
    });
}

fn deserialize_v1_bincode(payload: &[u8]) -> Result<TableSchema> {
    if let Ok(s) = bincode::deserialize::<TableSchema>(payload) {
        warn_v1_once(3);
        return Ok(s);
    }
    if let Ok(s) = bincode::deserialize::<V1Era3Schema>(payload) {
        warn_v1_once(3);
        return Ok(s.into());
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
        "Failed to deserialize V1 bincode schema (tried era-4/current, era-3/pre-index-constraint-bit, era-2/pre-collation, era-1/pre-index-state)"
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

// Era 3: has IndexDef.state + ColumnDef.collation, no IndexDef.is_constraint
#[derive(serde::Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
struct V1Era3IndexDef {
    pub name: String,
    pub id: u64,
    pub columns: Vec<String>,
    pub unique: bool,
    pub method: Option<String>,
    pub predicate: Option<String>,
    pub expressions: Vec<String>,
    pub state: crate::worker::types::IndexState,
}

#[derive(serde::Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
struct V1Era3Schema {
    pub name: String,
    pub table_id: u64,
    pub columns: Vec<crate::model::ColumnDef>,
    pub version: u64,
    pub pk_constraint_name: Option<String>,
    pub pk_indices: Vec<usize>,
    pub indexes: Vec<V1Era3IndexDef>,
    pub check_constraints: Vec<crate::model::CheckConstraint>,
    pub foreign_keys: Vec<crate::model::ForeignKeyConstraint>,
    pub owner: String,
    pub from_alias: Option<String>,
}

impl From<V1Era3Schema> for TableSchema {
    fn from(s: V1Era3Schema) -> Self {
        use crate::model::IndexDef;
        let V1Era3Schema {
            name,
            table_id,
            columns,
            version,
            pk_constraint_name,
            pk_indices,
            indexes,
            check_constraints,
            foreign_keys,
            owner,
            from_alias,
        } = s;
        let decoded_indexes: Vec<IndexDef> = indexes
            .into_iter()
            .map(|idx| {
                let V1Era3IndexDef {
                    name: idx_name,
                    id,
                    columns,
                    unique,
                    method,
                    predicate,
                    expressions,
                    state,
                } = idx;
                let is_constraint = legacy_index_constraint_default(unique, &columns);
                IndexDef {
                    name: idx_name,
                    id,
                    columns,
                    unique,
                    is_constraint,
                    method,
                    predicate,
                    expressions,
                    state,
                    cached_predicate_conjuncts: None,
                }
            })
            .collect();

        TableSchema {
            name,
            table_id,
            columns,
            version,
            pk_constraint_name,
            pk_indices,
            indexes: decoded_indexes,
            check_constraints,
            foreign_keys,
            owner,
            from_alias,
        }
    }
}

// Era 2: has IndexDef.state, no ColumnDef.collation
#[derive(serde::Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
struct V1Era2IndexDef {
    pub name: String,
    pub id: u64,
    pub columns: Vec<String>,
    pub unique: bool,
    pub method: Option<String>,
    pub predicate: Option<String>,
    pub expressions: Vec<String>,
    pub state: crate::worker::types::IndexState,
}

// Era 2: has IndexDef.state, no ColumnDef.collation and no IndexDef.is_constraint
#[derive(serde::Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
struct V1Era2Schema {
    pub name: String,
    pub table_id: u64,
    pub columns: Vec<V1ColumnDef>,
    pub version: u64,
    pub pk_constraint_name: Option<String>,
    pub pk_indices: Vec<usize>,
    pub indexes: Vec<V1Era2IndexDef>,
    pub check_constraints: Vec<crate::model::CheckConstraint>,
    pub foreign_keys: Vec<crate::model::ForeignKeyConstraint>,
    pub owner: String,
}

impl From<V1Era2Schema> for TableSchema {
    fn from(s: V1Era2Schema) -> Self {
        use crate::model::IndexDef;
        let V1Era2Schema {
            name,
            table_id,
            columns,
            version,
            pk_constraint_name,
            pk_indices,
            indexes,
            check_constraints,
            foreign_keys,
            owner,
        } = s;
        let decoded_columns: Vec<crate::model::ColumnDef> =
            columns.into_iter().map(Into::into).collect();
        let decoded_indexes: Vec<IndexDef> = indexes
            .into_iter()
            .map(|idx| {
                let V1Era2IndexDef {
                    name: idx_name,
                    id,
                    columns,
                    unique,
                    method,
                    predicate,
                    expressions,
                    state,
                } = idx;
                let is_constraint = legacy_index_constraint_default(unique, &columns);
                IndexDef {
                    name: idx_name,
                    id,
                    columns,
                    unique,
                    is_constraint,
                    method,
                    predicate,
                    expressions,
                    state,
                    cached_predicate_conjuncts: None,
                }
            })
            .collect();

        TableSchema {
            name,
            table_id,
            columns: decoded_columns,
            version,
            pk_constraint_name,
            pk_indices,
            indexes: decoded_indexes,
            check_constraints,
            foreign_keys,
            owner,
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
        let V1Era1Schema {
            name,
            table_id,
            columns,
            version,
            pk_constraint_name,
            pk_indices,
            indexes,
            check_constraints,
            foreign_keys,
            owner,
        } = s;
        let decoded_columns: Vec<crate::model::ColumnDef> =
            columns.into_iter().map(Into::into).collect();
        let decoded_indexes: Vec<IndexDef> = indexes
            .into_iter()
            .map(|idx| {
                let V1IndexDef {
                    name: idx_name,
                    id,
                    columns,
                    unique,
                    method,
                    predicate,
                    expressions,
                } = idx;
                let is_constraint = legacy_index_constraint_default(unique, &columns);
                IndexDef {
                    name: idx_name,
                    id,
                    columns,
                    unique,
                    is_constraint,
                    method,
                    predicate,
                    expressions,
                    state: IndexState::Ready,
                    cached_predicate_conjuncts: None,
                }
            })
            .collect();

        TableSchema {
            name,
            table_id,
            columns: decoded_columns,
            version,
            pk_constraint_name,
            pk_indices,
            indexes: decoded_indexes,
            check_constraints,
            foreign_keys,
            owner,
            from_alias: None,
        }
    }
}

fn legacy_index_constraint_default(unique: bool, index_columns: &[String]) -> bool {
    // Legacy eras (1/2/3) and old V2 payloads do not persist `is_constraint`.
    // Preserve historical behavior to avoid silently dropping compatibility:
    // any legacy UNIQUE index with key columns remains visible as a UNIQUE
    // constraint until rewritten with explicit `is_constraint`.
    unique && !index_columns.is_empty()
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
                is_constraint: false,
                method: Some("btree".into()),
                predicate: None,
                expressions: vec![],
                state: IndexState::Ready,
                cached_predicate_conjuncts: None,
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
    fn v1_bincode_era3_without_is_constraint_defaults_compatibly() {
        #[derive(serde::Serialize)]
        struct LegacyIndexDef {
            name: String,
            id: u64,
            columns: Vec<String>,
            unique: bool,
            method: Option<String>,
            predicate: Option<String>,
            expressions: Vec<String>,
            state: IndexState,
        }

        #[derive(serde::Serialize)]
        struct LegacyEra3Schema {
            name: String,
            table_id: u64,
            columns: Vec<ColumnDef>,
            version: u64,
            pk_constraint_name: Option<String>,
            pk_indices: Vec<usize>,
            indexes: Vec<LegacyIndexDef>,
            check_constraints: Vec<crate::model::CheckConstraint>,
            foreign_keys: Vec<crate::model::ForeignKeyConstraint>,
            owner: String,
            from_alias: Option<String>,
        }

        let legacy = LegacyEra3Schema {
            name: "public.legacy".into(),
            table_id: 9,
            columns: vec![ColumnDef {
                name: "id".into(),
                data_type: DataType::Int64,
                nullable: false,
                primary_key: true,
                unique: true,
                is_serial: false,
                default_expr: None,
                collation: None,
            }],
            version: 1,
            pk_constraint_name: Some("legacy_pkey".into()),
            pk_indices: vec![0],
            indexes: vec![LegacyIndexDef {
                name: "legacy_id_key".into(),
                id: 1,
                columns: vec!["id".into()],
                unique: true,
                method: Some("btree".into()),
                predicate: None,
                expressions: vec![],
                state: IndexState::Ready,
            }],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: "admin".into(),
            from_alias: None,
        };

        let payload = bincode::serialize(&legacy).unwrap();
        let mut data = Vec::from(SCHEMA_MAGIC_V1);
        data.extend_from_slice(&payload);

        let decoded = deserialize_schema(&data).unwrap();
        assert_eq!(decoded.name, "public.legacy");
        assert!(decoded.indexes[0].is_constraint);
    }

    #[test]
    fn v1_bincode_era3_plain_unique_index_defaults_to_constraint_for_backward_compat() {
        #[derive(serde::Serialize)]
        struct LegacyIndexDef {
            name: String,
            id: u64,
            columns: Vec<String>,
            unique: bool,
            method: Option<String>,
            predicate: Option<String>,
            expressions: Vec<String>,
            state: IndexState,
        }

        #[derive(serde::Serialize)]
        struct LegacyEra3Schema {
            name: String,
            table_id: u64,
            columns: Vec<ColumnDef>,
            version: u64,
            pk_constraint_name: Option<String>,
            pk_indices: Vec<usize>,
            indexes: Vec<LegacyIndexDef>,
            check_constraints: Vec<crate::model::CheckConstraint>,
            foreign_keys: Vec<crate::model::ForeignKeyConstraint>,
            owner: String,
            from_alias: Option<String>,
        }

        let legacy = LegacyEra3Schema {
            name: "public.legacy".into(),
            table_id: 9,
            columns: vec![ColumnDef {
                name: "id".into(),
                data_type: DataType::Int64,
                nullable: false,
                primary_key: true,
                unique: true,
                is_serial: false,
                default_expr: None,
                collation: None,
            }],
            version: 1,
            pk_constraint_name: Some("legacy_pkey".into()),
            pk_indices: vec![0],
            indexes: vec![LegacyIndexDef {
                name: "legacy_id_uix".into(),
                id: 1,
                columns: vec!["id".into()],
                unique: true,
                method: Some("btree".into()),
                predicate: None,
                expressions: vec![],
                state: IndexState::Ready,
            }],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: "admin".into(),
            from_alias: None,
        };

        let payload = bincode::serialize(&legacy).unwrap();
        let mut data = Vec::from(SCHEMA_MAGIC_V1);
        data.extend_from_slice(&payload);

        let decoded = deserialize_schema(&data).unwrap();
        assert_eq!(decoded.name, "public.legacy");
        assert!(decoded.indexes[0].is_constraint);
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
            indexes: vec![V1Era2IndexDef {
                name: "old_table_col1_key".into(),
                id: 1,
                columns: vec!["col1".into()],
                unique: true,
                method: Some("btree".into()),
                predicate: None,
                expressions: vec![],
                state: IndexState::Ready,
            }],
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
        assert_eq!(decoded.indexes.len(), 1);
        assert_eq!(decoded.indexes[0].name, "old_table_col1_key");
        assert!(decoded.indexes[0].is_constraint);
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
        assert!(decoded.indexes[0].is_constraint);
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
    fn v2_missing_index_constraint_bit_uses_legacy_backward_compat_default() {
        let schema = TableSchema {
            name: "public.legacy".into(),
            table_id: 11,
            columns: vec![
                ColumnDef {
                    name: "id".into(),
                    data_type: DataType::Int64,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    collation: None,
                },
                ColumnDef {
                    name: "email".into(),
                    data_type: DataType::Text,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    collation: None,
                },
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![
                IndexDef {
                    name: "legacy_id_uix".into(),
                    id: 1,
                    columns: vec!["id".into()],
                    unique: true,
                    is_constraint: false,
                    method: Some("btree".into()),
                    predicate: None,
                    expressions: vec![],
                    state: IndexState::Ready,
                    cached_predicate_conjuncts: None,
                },
                IndexDef {
                    name: "legacy_email_key".into(),
                    id: 2,
                    columns: vec!["email".into()],
                    unique: true,
                    is_constraint: true,
                    method: Some("btree".into()),
                    predicate: None,
                    expressions: vec![],
                    state: IndexState::Ready,
                    cached_predicate_conjuncts: None,
                },
                IndexDef {
                    name: "legacy_lower_email_uix".into(),
                    id: 3,
                    columns: vec![],
                    unique: true,
                    is_constraint: false,
                    method: Some("btree".into()),
                    predicate: None,
                    expressions: vec!["lower(email)".into()],
                    state: IndexState::Ready,
                    cached_predicate_conjuncts: None,
                },
            ],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: "postgres".into(),
            from_alias: None,
        };

        let mut json = serde_json::to_value(&schema).unwrap();
        let indexes = json
            .as_object_mut()
            .unwrap()
            .get_mut("indexes")
            .and_then(serde_json::Value::as_array_mut)
            .unwrap();
        for index in indexes {
            index.as_object_mut().unwrap().remove("is_constraint");
        }

        let payload = rmp_serde::to_vec_named(json.as_object().unwrap()).unwrap();
        let mut data = Vec::from(SCHEMA_MAGIC_V2);
        data.extend_from_slice(&payload);

        let decoded = deserialize_schema(&data).unwrap();
        let plain = decoded
            .indexes
            .iter()
            .find(|idx| idx.name == "legacy_id_uix")
            .unwrap();
        let constraint = decoded
            .indexes
            .iter()
            .find(|idx| idx.name == "legacy_email_key")
            .unwrap();
        let expression = decoded
            .indexes
            .iter()
            .find(|idx| idx.name == "legacy_lower_email_uix")
            .unwrap();
        assert!(plain.is_constraint);
        assert!(constraint.is_constraint);
        assert!(!expression.is_constraint);
    }

    #[test]
    fn schema_deser_cache_prevents_reparse() {
        SCHEMA_DESER_CACHE.clear();
        let data = serialize_schema(&sample_schema()).unwrap();

        // First call: cache miss → full parse + hydrate + insert.
        let first = deserialize_schema(&data).unwrap();
        assert_eq!(first.name, "public.users");
        assert!(
            SCHEMA_DESER_CACHE.contains_key(&data),
            "entry must be cached after first deserialize"
        );

        // Mutate the cached entry so we can distinguish a cache hit from
        // a fresh parse (fresh parse would return "public.users").
        SCHEMA_DESER_CACHE.get_mut(&data).unwrap().name = "CACHE_HIT".to_string();

        // Second call: same bytes → must return the mutated cached entry.
        let second = deserialize_schema(&data).unwrap();
        assert_eq!(
            second.name, "CACHE_HIT",
            "second call must return cached entry, not re-parse"
        );

        SCHEMA_DESER_CACHE.clear();
    }

    #[test]
    fn bad_magic_rejected() {
        assert!(deserialize_schema(b"GARBAGE\0payload").is_err());
    }
}
