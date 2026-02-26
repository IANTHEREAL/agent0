use super::helpers::{bool_col, int_col, int_val, text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema, Value};
use anyhow::Result;
use async_trait::async_trait;

pub struct PgOpclass;

/// Static opclass entries.  OIDs are synthetic but internally consistent
/// with `pg_index.indclass` values produced by `pg_index.rs`.
pub(crate) struct OpclassEntry {
    pub(crate) oid: i64,
    pub(crate) opcname: &'static str,
    pub(crate) opcdefault: bool,
    pub(crate) opcmethod: i64, // AM OID (403=btree, 405=hash, 783=gist, 2742=gin)
    pub(crate) opcintype: i64, // pg_type OID of the indexed type
}

/// Minimal set of default opclasses that SQLAlchemy reflection queries
/// may encounter via `pg_opclass.oid = unnest(indclass)`.
/// Each access method gets one "catch-all default" entry plus entries
/// for common PostgreSQL types.
pub(crate) static OPCLASS_ENTRIES: &[OpclassEntry] = &[
    // btree opclasses — one default per common type
    OpclassEntry {
        oid: 10042,
        opcname: "int8_ops",
        opcdefault: true,
        opcmethod: 403,
        opcintype: 20,
    },
    OpclassEntry {
        oid: 10043,
        opcname: "int4_ops",
        opcdefault: true,
        opcmethod: 403,
        opcintype: 23,
    },
    OpclassEntry {
        oid: 10044,
        opcname: "int2_ops",
        opcdefault: true,
        opcmethod: 403,
        opcintype: 21,
    },
    OpclassEntry {
        oid: 10045,
        opcname: "text_ops",
        opcdefault: true,
        opcmethod: 403,
        opcintype: 25,
    },
    OpclassEntry {
        oid: 10046,
        opcname: "bool_ops",
        opcdefault: true,
        opcmethod: 403,
        opcintype: 16,
    },
    OpclassEntry {
        oid: 10047,
        opcname: "timestamptz_ops",
        opcdefault: true,
        opcmethod: 403,
        opcintype: 1184,
    },
    OpclassEntry {
        oid: 10048,
        opcname: "timestamp_ops",
        opcdefault: true,
        opcmethod: 403,
        opcintype: 1114,
    },
    OpclassEntry {
        oid: 10049,
        opcname: "float8_ops",
        opcdefault: true,
        opcmethod: 403,
        opcintype: 701,
    },
    OpclassEntry {
        oid: 10050,
        opcname: "float4_ops",
        opcdefault: true,
        opcmethod: 403,
        opcintype: 700,
    },
    OpclassEntry {
        oid: 10051,
        opcname: "numeric_ops",
        opcdefault: true,
        opcmethod: 403,
        opcintype: 1700,
    },
    OpclassEntry {
        oid: 10052,
        opcname: "uuid_ops",
        opcdefault: true,
        opcmethod: 403,
        opcintype: 2950,
    },
    OpclassEntry {
        oid: 10053,
        opcname: "date_ops",
        opcdefault: true,
        opcmethod: 403,
        opcintype: 1082,
    },
    OpclassEntry {
        oid: 10054,
        opcname: "varchar_ops",
        opcdefault: true,
        opcmethod: 403,
        opcintype: 1043,
    },
    OpclassEntry {
        oid: 10055,
        opcname: "name_ops",
        opcdefault: true,
        opcmethod: 403,
        opcintype: 19,
    },
    OpclassEntry {
        oid: 10056,
        opcname: "bytea_ops",
        opcdefault: true,
        opcmethod: 403,
        opcintype: 17,
    },
    OpclassEntry {
        oid: 10057,
        opcname: "interval_ops",
        opcdefault: true,
        opcmethod: 403,
        opcintype: 1186,
    },
    OpclassEntry {
        oid: 10058,
        opcname: "jsonb_ops",
        opcdefault: true,
        opcmethod: 403,
        opcintype: 3802,
    },
    // hash opclasses
    OpclassEntry {
        oid: 10080,
        opcname: "int8_ops",
        opcdefault: true,
        opcmethod: 405,
        opcintype: 20,
    },
    OpclassEntry {
        oid: 10081,
        opcname: "int4_ops",
        opcdefault: true,
        opcmethod: 405,
        opcintype: 23,
    },
    OpclassEntry {
        oid: 10082,
        opcname: "text_ops",
        opcdefault: true,
        opcmethod: 405,
        opcintype: 25,
    },
    // gist opclasses
    OpclassEntry {
        oid: 10088,
        opcname: "tsvector_ops",
        opcdefault: true,
        opcmethod: 783,
        opcintype: 3614,
    },
    // gin opclasses
    OpclassEntry {
        oid: 10096,
        opcname: "jsonb_ops",
        opcdefault: true,
        opcmethod: 2742,
        opcintype: 3802,
    },
    OpclassEntry {
        oid: 10097,
        opcname: "tsvector_ops",
        opcdefault: true,
        opcmethod: 2742,
        opcintype: 3614,
    },
    OpclassEntry {
        oid: 10098,
        opcname: "jsonb_path_ops",
        opcdefault: false,
        opcmethod: 2742,
        opcintype: 3802,
    },
    OpclassEntry {
        oid: 10099,
        opcname: "array_ops",
        opcdefault: true,
        opcmethod: 2742,
        opcintype: 2277,
    },
];

#[async_trait]
impl VirtualTable for PgOpclass {
    fn name(&self) -> &str {
        "pg_opclass"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "pg_opclass".to_string(),
            columns: vec![
                int_col("oid"),
                text_col("opcname"),
                bool_col("opcdefault"),
                int_col("opcmethod"),
                int_col("opcintype"),
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        }
    }

    async fn scan(&self, _ctx: &mut ScanContext<'_>) -> Result<Vec<Row>> {
        Ok(OPCLASS_ENTRIES
            .iter()
            .map(|e| {
                Row::new(vec![
                    int_val(e.oid),
                    text_val(e.opcname),
                    Value::Boolean(e.opcdefault),
                    int_val(e.opcmethod),
                    int_val(e.opcintype),
                ])
            })
            .collect())
    }
}
