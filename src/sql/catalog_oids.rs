use anyhow::{anyhow, Result};

const TABLE_OID_BASE: i64 = 10_000_000_000;
const INDEX_OID_BASE: i64 = 20_000_000_000;
const SEQUENCE_OID_BASE: i64 = 30_000_000_000;
const VIEW_OID_BASE: i64 = 40_000_000_000;
const FUNCTION_OID_BASE: i64 = 50_000_000_000;
const BUILTIN_FUNCTION_OID_BASE: i64 = 55_000_000_000;
const TRIGGER_OID_BASE: i64 = 60_000_000_000;
const ATTRDEF_OID_BASE: i64 = 70_000_000_000;
const ROLE_OID_BASE: i64 = 80_000_000_000;

const INDEX_ID_BITS: u32 = 20;
const MAX_INDEX_ID: u64 = (1u64 << INDEX_ID_BITS) - 1;

fn to_i64(v: u64, what: &str) -> Result<i64> {
    i64::try_from(v).map_err(|_| anyhow!("{} overflow", what))
}

pub fn pg_class_table_oid(table_id: u64) -> Result<i64> {
    TABLE_OID_BASE
        .checked_add(to_i64(table_id, "table_id")?)
        .ok_or_else(|| anyhow!("pg_class table oid overflow"))
}

pub fn pg_class_index_oid(table_id: u64, index_id: u64) -> Result<i64> {
    if index_id > MAX_INDEX_ID {
        return Err(anyhow!(
            "index_id {} is too large for stable catalog OID packing",
            index_id
        ));
    }
    let table_part = to_i64(table_id, "table_id")?
        .checked_shl(INDEX_ID_BITS)
        .ok_or_else(|| anyhow!("pg_class index oid overflow"))?;
    INDEX_OID_BASE
        .checked_add(table_part)
        .and_then(|v| v.checked_add(index_id as i64))
        .ok_or_else(|| anyhow!("pg_class index oid overflow"))
}

pub fn pg_class_pk_index_oid(table_id: u64) -> Result<i64> {
    pg_class_index_oid(table_id, 0)
}

pub fn pg_class_sequence_oid(sequence_oid: u32) -> i64 {
    SEQUENCE_OID_BASE + sequence_oid as i64
}

pub fn pg_class_view_oid(view_oid: u32) -> i64 {
    VIEW_OID_BASE + view_oid as i64
}

pub fn pg_proc_function_oid(function_oid: u32) -> i64 {
    FUNCTION_OID_BASE + function_oid as i64
}

/// Overload-aware OID derivation for a builtin function. PG's pg_proc
/// assigns a distinct oid per (proname, proargtypes) row — DB9 mirrors
/// that by folding the argument-type OIDs into the hash. Two overloads
/// of the same name (SIGN's double-precision and numeric forms, ABS's
/// six integer/float/numeric forms, ...) therefore get distinct oids
/// and `SELECT oid FROM pg_proc WHERE proname='sign'` returns two
/// values — matching PG's introspection contract.
pub fn pg_builtin_function_overload_oid(function_name: &str, arg_type_oids: &[i64]) -> i64 {
    let lower = function_name.to_ascii_lowercase();
    let mut hasher_bytes = Vec::with_capacity(lower.len() + 1 + arg_type_oids.len() * 8);
    hasher_bytes.extend_from_slice(lower.as_bytes());
    hasher_bytes.push(b'(');
    for oid in arg_type_oids {
        hasher_bytes.extend_from_slice(&oid.to_le_bytes());
    }
    BUILTIN_FUNCTION_OID_BASE + (fnv1a_64(&hasher_bytes) % 1_000_000_000) as i64
}

pub fn pg_trigger_oid(trigger_oid: u32) -> i64 {
    TRIGGER_OID_BASE + trigger_oid as i64
}

pub fn pg_attrdef_oid(table_id: u64, attnum: u32) -> Result<i64> {
    let table_part = to_i64(table_id, "table_id")?
        .checked_mul(10_000)
        .ok_or_else(|| anyhow!("pg_attrdef oid overflow"))?;
    ATTRDEF_OID_BASE
        .checked_add(table_part)
        .and_then(|v| v.checked_add(attnum as i64))
        .ok_or_else(|| anyhow!("pg_attrdef oid overflow"))
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut hash = FNV_OFFSET;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

pub fn pg_role_oid(role_name: &str) -> i64 {
    if role_name.eq_ignore_ascii_case("postgres") {
        return 10;
    }

    const ROLE_HASH_RANGE: u64 = 1_000_000_000;
    let h = fnv1a_64(role_name.as_bytes());
    ROLE_OID_BASE + (h % ROLE_HASH_RANGE) as i64
}

/// Fixed PostgreSQL bootstrap OIDs for core `pg_catalog` relations.
///
/// These are authoritative PostgreSQL catalog contracts, not db9-generated IDs.
/// Keep this as the single source of truth for places that need to resolve
/// well-known virtual catalog relations to their real PostgreSQL OIDs.
pub fn pg_catalog_relation_oid(name: &str) -> Option<i64> {
    match name {
        "pg_class" => Some(1259),
        "pg_type" => Some(1247),
        "pg_attribute" => Some(1249),
        "pg_proc" => Some(1255),
        "pg_namespace" => Some(2615),
        "pg_constraint" => Some(2606),
        "pg_attrdef" => Some(2604),
        "pg_index" => Some(2610),
        "pg_database" => Some(1262),
        "pg_tablespace" => Some(1213),
        "pg_description" => Some(2609),
        "pg_shdescription" => Some(2396),
        "pg_extension" => Some(3079),
        "pg_am" => Some(2601),
        "pg_trigger" => Some(2620),
        "pg_depend" => Some(2608),
        "pg_roles" => Some(12000),
        "pg_authid" => Some(1260),
        "pg_collation" => Some(3456),
        "pg_enum" => Some(3501),
        "pg_sequence" => Some(2224),
        _ => None,
    }
}

pub fn pg_catalog_relation_name(oid: i64) -> Option<&'static str> {
    match oid {
        1259 => Some("pg_class"),
        1247 => Some("pg_type"),
        1249 => Some("pg_attribute"),
        1255 => Some("pg_proc"),
        2615 => Some("pg_namespace"),
        2606 => Some("pg_constraint"),
        2604 => Some("pg_attrdef"),
        2610 => Some("pg_index"),
        1262 => Some("pg_database"),
        1213 => Some("pg_tablespace"),
        2609 => Some("pg_description"),
        2396 => Some("pg_shdescription"),
        3079 => Some("pg_extension"),
        2601 => Some("pg_am"),
        2620 => Some("pg_trigger"),
        2608 => Some("pg_depend"),
        12000 => Some("pg_roles"),
        1260 => Some("pg_authid"),
        3456 => Some("pg_collation"),
        3501 => Some("pg_enum"),
        2224 => Some("pg_sequence"),
        _ => None,
    }
}
