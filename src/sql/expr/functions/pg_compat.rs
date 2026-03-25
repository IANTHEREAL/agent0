use crate::model::{DataType, Value};
use anyhow::Result;
use std::collections::HashMap;

use super::SqlFn;

mod binary;
mod misc;
mod regtype;

#[cfg(test)]
use {crate::sql::pg_types, binary::*, misc::*, regtype::*};

pub(crate) use misc::pg_typeof_name_for_datatype;
#[cfg(test)]
pub(crate) use regtype::strip_regtype_array_dims;
pub(crate) use regtype::{
    parse_regtype_lookup, resolve_builtin_regtype_lookup, validate_resolved_regtype_typmod,
    ParsedRegtypeLookup, ParsedRegtypeLookupKind,
};

pub fn register(map: &mut HashMap<&'static str, SqlFn>) {
    map.insert("PG_TYPEOF", misc::pg_typeof);
    map.insert("PG_COLUMN_SIZE", misc::pg_column_size);
    map.insert("FORMAT_TYPE", misc::format_type);
    map.insert("TO_REGTYPE", regtype::to_regtype);
    map.insert("PG_IS_IN_RECOVERY", misc::pg_is_in_recovery);
    map.insert("PG_TABLE_IS_VISIBLE", misc::pg_table_is_visible);
    map.insert("PG_TYPE_IS_VISIBLE", misc::pg_type_is_visible);
    map.insert("CLOCK_TIMESTAMP", misc::clock_timestamp);
    map.insert("TXID_CURRENT", misc::txid_current);
    map.insert("PG_ENCODING_TO_CHAR", misc::pg_encoding_to_char);
    map.insert("OBJ_DESCRIPTION", misc::obj_description);
    map.insert("COL_DESCRIPTION", misc::obj_description);
    map.insert("SHOBJ_DESCRIPTION", misc::obj_description);
    map.insert("PG_GET_EXPR", misc::pg_get_expr);
    map.insert(
        "PG_GET_STATISTICSOBJDEF_COLUMNS",
        misc::pg_get_statisticsobjdef_columns,
    );
    map.insert("HAS_SCHEMA_PRIVILEGE", misc::has_privilege);
    map.insert("HAS_TABLE_PRIVILEGE", misc::has_privilege);
    map.insert("HAS_DATABASE_PRIVILEGE", misc::has_privilege);
    map.insert(
        "PG_RELATION_IS_PUBLISHABLE",
        misc::pg_relation_is_publishable,
    );
    map.insert("PG_PARTITION_ANCESTORS", misc::pg_partition_ancestors);
    map.insert("INT4SEND", binary::int4send);
    map.insert("INT8SEND", binary::int8send);
    map.insert("UUID_SEND", binary::uuid_send);
    map.insert("SET_BIT", binary::set_bit_bytea);
    map.insert("GET_BIT", binary::get_bit_bytea);
    map.insert("HASHTEXT", misc::hashtext);
}

#[cfg(test)]
mod tests;
