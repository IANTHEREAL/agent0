//! Schema/function/row serialization with versioning.
//!
//! ## Schema wire format
//!
//! ```text
//! V2:  b"DB9_SCHEMA_V2\0" ++ MessagePack (named map)  ← only supported format
//! V1:  b"DB9_SCHEMA_V1\0" ++ bincode payload          ← rejected with clear error
//! ```
//!
//! V2 uses MessagePack named-map mode so `#[serde(default)]` works
//! natively — future field additions need zero legacy structs.

use crate::model::{default_owner, FunctionDef, MatViewDef, Row, TableSchema, ViewDef};
use anyhow::{Context, Result};
use dashmap::DashMap;
use sqlparser::ast::Statement;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use std::sync::LazyLock;

const SCHEMA_MAGIC_V2: &[u8] = b"DB9_SCHEMA_V2\0";
const SCHEMA_MAGIC_V1: &[u8] = b"DB9_SCHEMA_V1\0"; // CI-ALLOWED: kept for error detection

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

    let payload = if let Some(p) = data.strip_prefix(SCHEMA_MAGIC_V2) {
        p
    } else if data.starts_with(SCHEMA_MAGIC_V1) {
        anyhow::bail!(
            "V1 bincode schema detected. This server requires V2 msgpack format. \
             Please recreate the database."
        );
    } else {
        anyhow::bail!("Schema data missing magic header (expected DB9_SCHEMA_V2)")
    };
    let mut schema = deserialize_v2_msgpack(payload)?;
    schema.hydrate_runtime_caches();

    // Populate cache. Clear if too large to bound memory usage.
    if SCHEMA_DESER_CACHE.len() >= MAX_SCHEMA_CACHE_ENTRIES {
        SCHEMA_DESER_CACHE.clear();
    }
    SCHEMA_DESER_CACHE.insert(data.to_vec(), schema.clone());

    Ok(schema)
}

fn deserialize_v2_msgpack(payload: &[u8]) -> Result<TableSchema> {
    let decoded: TableSchema =
        rmp_serde::from_slice(payload).context("Failed to deserialize V2 MessagePack schema")?;
    Ok(decoded)
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

// DB9_FUNCTION_V1 bincode eras (newest first):
// - current: +security_definer
// - era 1: pre-security-definer
// - era 0: pre-owner
#[derive(serde::Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
struct FunctionDefEra1 {
    #[serde(default)]
    oid: u32,
    schema: String,
    name: String,
    arg_types: Vec<String>,
    return_type: String,
    language: String,
    body: String,
    owner: String,
}

impl From<FunctionDefEra1> for FunctionDef {
    fn from(legacy: FunctionDefEra1) -> Self {
        Self {
            oid: legacy.oid,
            schema: legacy.schema,
            name: legacy.name,
            arg_types: legacy.arg_types,
            return_type: legacy.return_type,
            language: legacy.language,
            body: legacy.body,
            owner: legacy.owner,
            security_definer: false,
        }
    }
}

#[derive(serde::Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
struct FunctionDefEra0 {
    #[serde(default)]
    oid: u32,
    schema: String,
    name: String,
    arg_types: Vec<String>,
    return_type: String,
    language: String,
    body: String,
}

impl From<FunctionDefEra0> for FunctionDef {
    fn from(legacy: FunctionDefEra0) -> Self {
        Self {
            oid: legacy.oid,
            schema: legacy.schema,
            name: legacy.name,
            arg_types: legacy.arg_types,
            return_type: legacy.return_type,
            language: legacy.language,
            body: legacy.body,
            owner: default_owner(),
            security_definer: false,
        }
    }
}

pub fn deserialize_function_def(data: &[u8]) -> Result<FunctionDef> {
    const FUNCTION_MAGIC: &[u8] = b"DB9_FUNCTION_V1\0";
    let payload = data.strip_prefix(FUNCTION_MAGIC).context(
        "Function data missing DB9_FUNCTION_V1 header (V1 legacy format no longer supported)",
    )?;
    if let Ok(def) = bincode::deserialize::<FunctionDef>(payload) {
        return Ok(def);
    }
    if let Ok(def) = bincode::deserialize::<FunctionDefEra1>(payload) {
        return Ok(def.into());
    }
    if let Ok(def) = bincode::deserialize::<FunctionDefEra0>(payload) {
        return Ok(def.into());
    }
    anyhow::bail!(
        "Failed to deserialize function definition (tried current, era-1/pre-security-definer, era-0/pre-owner)"
    )
}

pub fn serialize_view_def(def: &ViewDef) -> Result<Vec<u8>> {
    bincode::serialize(def).context("Failed to serialize view definition")
}

pub fn serialize_materialized_view_def(def: &MatViewDef) -> Result<Vec<u8>> {
    bincode::serialize(def).context("Failed to serialize matview definition")
}

// View definition bincode eras (newest first):
// - current: +owner +deps +security_definer
// - era 2: pre-security-definer
// - era 1: pre-owner
// - era 0: pre-deps
#[derive(serde::Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
struct ViewDefEra2 {
    oid: u32,
    schema: String,
    name: String,
    owner: String,
    query: String,
    deps: Vec<String>,
}

impl From<ViewDefEra2> for ViewDef {
    fn from(legacy: ViewDefEra2) -> Self {
        Self {
            oid: legacy.oid,
            schema: legacy.schema,
            name: legacy.name,
            owner: legacy.owner,
            query: legacy.query,
            deps: legacy.deps,
            security_definer: false,
        }
    }
}

#[derive(serde::Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
struct ViewDefEra1 {
    oid: u32,
    schema: String,
    name: String,
    query: String,
    deps: Vec<String>,
}

impl From<ViewDefEra1> for ViewDef {
    fn from(legacy: ViewDefEra1) -> Self {
        Self {
            oid: legacy.oid,
            schema: legacy.schema,
            name: legacy.name,
            owner: default_owner(),
            query: legacy.query,
            deps: legacy.deps,
            security_definer: false,
        }
    }
}

#[derive(serde::Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
struct ViewDefEra0 {
    #[serde(default)]
    oid: u32,
    schema: String,
    name: String,
    query: String,
}

impl From<ViewDefEra0> for ViewDef {
    fn from(legacy: ViewDefEra0) -> Self {
        Self {
            oid: legacy.oid,
            schema: legacy.schema,
            name: legacy.name,
            owner: default_owner(),
            query: legacy.query,
            deps: Vec::new(),
            security_definer: false,
        }
    }
}

pub fn deserialize_view_def(data: &[u8]) -> Result<ViewDef> {
    if let Ok(def) = bincode::deserialize::<ViewDef>(data) {
        return Ok(def);
    }
    if let Ok(def) = bincode::deserialize::<ViewDefEra2>(data) {
        return Ok(def.into());
    }
    if let Ok(def) = bincode::deserialize::<ViewDefEra1>(data) {
        return Ok(def.into());
    }
    if let Ok(def) = bincode::deserialize::<ViewDefEra0>(data) {
        return Ok(def.into());
    }
    anyhow::bail!(
        "Failed to deserialize view definition (tried current, era-2/pre-security-definer, era-1/pre-owner, era-0/pre-deps)"
    )
}

pub fn deserialize_materialized_view_def(data: &[u8], full_name: &str) -> Result<MatViewDef> {
    if let Ok(def) = bincode::deserialize::<MatViewDef>(data) {
        return Ok(def);
    }

    let query = decode_legacy_materialized_view_query(data)?;
    let (schema, name) = split_relation_name(full_name);
    Ok(MatViewDef {
        schema: schema.to_string(),
        name: name.to_string(),
        query,
        deps: Vec::new(),
    })
}

fn decode_legacy_materialized_view_query(data: &[u8]) -> Result<String> {
    let query = std::str::from_utf8(data)
        .context("Failed to deserialize matview definition as bincode or legacy UTF-8 SQL")?
        .to_string();
    let dialect = PostgreSqlDialect {};
    let stmts = Parser::parse_sql(&dialect, &query)
        .context("Failed to parse legacy materialized view SQL")?;
    match stmts.as_slice() {
        [Statement::Query(_)] => Ok(query),
        _ => anyhow::bail!("Legacy materialized view SQL is not a single query"),
    }
}

fn split_relation_name(full_name: &str) -> (&str, &str) {
    full_name.split_once('.').unwrap_or(("public", full_name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{default_owner, ColumnDef, DataType, IndexDef};
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
                    generation_expr: None,
                    generation_expr_authorized_by: None,
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
                    generation_expr: None,
                    generation_expr_authorized_by: None,
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
                hnsw_m: None,
                hnsw_ef_construction: None,
                hnsw_distance_metric: None,
            }],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: "admin".into(),
            rls_enabled: false,
            rls_force: false,
            from_alias: None,
        }
    }

    #[test]
    fn v2_msgpack_round_trip() {
        let schema = sample_schema();
        let data = serialize_schema(&schema).unwrap();
        assert!(data.starts_with(SCHEMA_MAGIC_V2));
        let decoded = deserialize_schema(&data).unwrap();
        assert_eq!(decoded.name, schema.name);
        assert_eq!(decoded.table_id, schema.table_id);
        assert_eq!(decoded.columns.len(), 2);
        assert_eq!(decoded.columns[1].collation, Some("en_US".into()));
        assert_eq!(decoded.indexes[0].state, IndexState::Ready);
    }

    #[test]
    fn v1_schema_rejected() {
        let mut data = Vec::from(SCHEMA_MAGIC_V1);
        data.extend_from_slice(b"some payload");
        let err = deserialize_schema(&data).unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("V1 bincode schema detected"),
            "expected V1 rejection error, got: {msg}"
        );
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
    fn hnsw_index_metadata_survives_v2_roundtrip() {
        let mut schema = sample_schema();
        schema.indexes[0].method = Some("hnsw".into());
        schema.indexes[0].hnsw_m = Some(32);
        schema.indexes[0].hnsw_ef_construction = Some(128);
        schema.indexes[0].hnsw_distance_metric = Some("ip".into());

        let data = serialize_schema(&schema).unwrap();
        let decoded = deserialize_schema(&data).unwrap();
        let index = &decoded.indexes[0];
        assert_eq!(index.method.as_deref(), Some("hnsw"));
        assert_eq!(index.hnsw_m, Some(32));
        assert_eq!(index.hnsw_ef_construction, Some(128));
        assert_eq!(index.hnsw_distance_metric.as_deref(), Some("ip"));
    }

    #[test]
    fn function_deserializer_accepts_pre_security_definer_bytes() {
        let mut data = Vec::from(b"DB9_FUNCTION_V1\0".as_slice());
        data.extend(
            bincode::serialize(&FunctionDefEra1 {
                oid: 42,
                schema: "public".into(),
                name: "legacy_func".into(),
                arg_types: vec!["int4".into()],
                return_type: "int4".into(),
                language: "sql".into(),
                body: "SELECT 1".into(),
                owner: "admin".into(),
            })
            .unwrap(),
        );

        let decoded = deserialize_function_def(&data).unwrap();
        assert_eq!(decoded.oid, 42);
        assert_eq!(decoded.owner, "admin");
        assert!(!decoded.security_definer);
    }

    #[test]
    fn function_deserializer_uses_canonical_owner_default_for_pre_owner_bytes() {
        let mut data = Vec::from(b"DB9_FUNCTION_V1\0".as_slice());
        data.extend(
            bincode::serialize(&FunctionDefEra0 {
                oid: 7,
                schema: "public".into(),
                name: "legacy_ownerless_func".into(),
                arg_types: vec![],
                return_type: "text".into(),
                language: "sql".into(),
                body: "SELECT 'ok'".into(),
            })
            .unwrap(),
        );

        let decoded = deserialize_function_def(&data).unwrap();
        assert_eq!(decoded.oid, 7);
        assert_eq!(decoded.owner, default_owner());
        assert!(!decoded.security_definer);
    }

    #[test]
    fn view_deserializer_accepts_pre_owner_bytes() {
        let data = bincode::serialize(&ViewDefEra1 {
            oid: 9,
            schema: "public".into(),
            name: "legacy_view".into(),
            query: "SELECT 1".into(),
            deps: vec!["public.t".into()],
        })
        .unwrap();

        let decoded = deserialize_view_def(&data).unwrap();
        assert_eq!(decoded.oid, 9);
        assert_eq!(decoded.owner, default_owner());
        assert_eq!(decoded.deps, vec!["public.t".to_string()]);
        assert!(!decoded.security_definer);
    }

    #[test]
    fn view_deserializer_accepts_pre_security_definer_bytes() {
        let data = bincode::serialize(&ViewDefEra2 {
            oid: 10,
            schema: "public".into(),
            name: "legacy_view_with_owner".into(),
            owner: "admin".into(),
            query: "SELECT 1".into(),
            deps: vec!["public.t".into()],
        })
        .unwrap();

        let decoded = deserialize_view_def(&data).unwrap();
        assert_eq!(decoded.oid, 10);
        assert_eq!(decoded.owner, "admin");
        assert_eq!(decoded.deps, vec!["public.t".to_string()]);
        assert!(!decoded.security_definer);
    }

    #[test]
    fn view_deserializer_accepts_pre_deps_bytes() {
        let data = bincode::serialize(&ViewDefEra0 {
            oid: 11,
            schema: "public".into(),
            name: "legacy_view_no_deps".into(),
            query: "SELECT 1".into(),
        })
        .unwrap();

        let decoded = deserialize_view_def(&data).unwrap();
        assert_eq!(decoded.oid, 11);
        assert_eq!(decoded.owner, default_owner());
        assert!(decoded.deps.is_empty());
        assert!(!decoded.security_definer);
    }

    #[test]
    fn materialized_view_deserializer_accepts_current_bytes() {
        let data = serialize_materialized_view_def(&MatViewDef {
            schema: "public".into(),
            name: "mv".into(),
            query: "SELECT 1".into(),
            deps: vec!["public.t".into()],
        })
        .unwrap();

        let decoded = deserialize_materialized_view_def(&data, "public.mv").unwrap();
        assert_eq!(decoded.schema, "public");
        assert_eq!(decoded.name, "mv");
        assert_eq!(decoded.query, "SELECT 1");
        assert_eq!(decoded.deps, vec!["public.t".to_string()]);
    }

    #[test]
    fn materialized_view_deserializer_accepts_legacy_plain_sql_bytes() {
        let data = b"SELECT * FROM public.t".to_vec();

        let decoded = deserialize_materialized_view_def(&data, "analytics.mv_sales").unwrap();
        assert_eq!(decoded.schema, "analytics");
        assert_eq!(decoded.name, "mv_sales");
        assert_eq!(decoded.query, "SELECT * FROM public.t");
        assert!(decoded.deps.is_empty());
    }

    #[test]
    fn materialized_view_deserializer_rejects_non_query_legacy_sql() {
        let err =
            deserialize_materialized_view_def(b"CREATE TABLE t(id INT)", "public.mv").unwrap_err();
        assert!(format!("{err}").contains("not a single query"));
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
                    generation_expr: None,
                    generation_expr_authorized_by: None,
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
                    generation_expr: None,
                    generation_expr_authorized_by: None,
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
                    hnsw_m: None,
                    hnsw_ef_construction: None,
                    hnsw_distance_metric: None,
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
                    hnsw_m: None,
                    hnsw_ef_construction: None,
                    hnsw_distance_metric: None,
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
                    hnsw_m: None,
                    hnsw_ef_construction: None,
                    hnsw_distance_metric: None,
                },
            ],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: "postgres".into(),
            rls_enabled: false,
            rls_force: false,
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
