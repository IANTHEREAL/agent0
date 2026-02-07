use anyhow::{anyhow, Result};

const TABLE_OID_BASE: i64 = 10_000_000_000;
const INDEX_OID_BASE: i64 = 20_000_000_000;
const SEQUENCE_OID_BASE: i64 = 30_000_000_000;
const VIEW_OID_BASE: i64 = 40_000_000_000;
const FUNCTION_OID_BASE: i64 = 50_000_000_000;
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
