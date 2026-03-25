use super::helpers::{
    bool_col, int2vector_col, int_col, int_val, null_val, oidvector_col, split_schema_and_name,
    text_col,
};
use super::{ScanContext, VirtualTable};
use crate::model::{DataType, Row, TableSchema, Value};
use crate::sql::catalog_oids;
use crate::sql::pg_types;
use anyhow::Result;
use async_trait::async_trait;

/// Return the default opclass OID for a given (access-method, pg_type OID) pair.
/// These values must match `pg_opclass` virtual table rows produced by `pg_opclass.rs`.
/// When the exact type is not catalogued, falls back to the first default entry for
/// the access method (preserving previous behaviour).
fn default_opclass_oid(method: Option<&str>, type_oid: i64) -> i64 {
    use super::pg_opclass::OPCLASS_ENTRIES;

    let am_oid: i64 = match method.unwrap_or("btree").to_ascii_lowercase().as_str() {
        "btree" => 403,
        "hash" => 405,
        "gin" => 2742,
        "gist" => 783,
        _ => 403,
    };

    // Exact match: same AM + same indexed type + default
    if let Some(e) = OPCLASS_ENTRIES
        .iter()
        .find(|e| e.opcmethod == am_oid && e.opcintype == type_oid && e.opcdefault)
    {
        return e.oid;
    }
    // Fallback: first default entry for the AM
    OPCLASS_ENTRIES
        .iter()
        .find(|e| e.opcmethod == am_oid && e.opcdefault)
        .map(|e| e.oid)
        .unwrap_or(10042)
}

/// Resolve indexed-column type OID for opclass lookup.
/// We use `anyarray` OID for arrays so GIN defaults map to `array_ops`.
fn opclass_input_type_oid(dt: &DataType) -> i64 {
    match dt {
        DataType::Array(_) => pg_types::OID_ANYARRAY,
        _ => pg_types::oid_and_typlen_for_datatype(dt).0,
    }
}

fn index_predicate_value(predicate: Option<&str>) -> Value {
    predicate
        .map(|pred| Value::Text(pred.to_string()))
        .unwrap_or_else(null_val)
}

pub struct PgIndex;

#[async_trait]
impl VirtualTable for PgIndex {
    fn name(&self) -> &str {
        "pg_index"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn schema(&self) -> TableSchema {
        TableSchema::virtual_table(
            "pg_index",
            vec![
                int_col("indexrelid"),
                int_col("indrelid"),
                int_col("indnatts"),
                bool_col("indisunique"),
                bool_col("indisprimary"),
                bool_col("indisexclusion"),
                bool_col("indimmediate"),
                bool_col("indisclustered"),
                bool_col("indisvalid"),
                bool_col("indisreplident"),
                int2vector_col("indkey"),
                text_col("indexprs"),
                text_col("indpred"),
                int_col("indnkeyatts"),
                bool_col("indnullsnotdistinct"),
                oidvector_col("indclass"),
                int2vector_col("indoption"),
            ],
        )
    }

    async fn scan(&self, ctx: &mut ScanContext<'_>) -> Result<Vec<Row>> {
        let mut rows = Vec::new();

        for full_table_name in ctx.user_tables {
            let (_table_schema, _table_name) = split_schema_and_name(full_table_name);
            if let Some(schema) = ctx
                .store
                .get_schema(ctx.txn, ctx.db_id, full_table_name)
                .await?
            {
                let base_table_oid = catalog_oids::pg_class_table_oid(schema.table_id)?;

                for idx in &schema.indexes {
                    let index_oid = catalog_oids::pg_class_index_oid(schema.table_id, idx.id)?;

                    let index_col_count = idx.columns.len() + idx.expressions.len();
                    let mut col_indices: Vec<i64> = Vec::new();
                    for col_name in &idx.columns {
                        if let Some(pos) = schema.columns.iter().position(|c| &c.name == col_name) {
                            col_indices.push((pos + 1) as i64);
                        }
                    }
                    col_indices.extend(std::iter::repeat_n(0, idx.expressions.len()));
                    let indkey =
                        Value::Array(col_indices.iter().map(|i| Value::Int64(*i)).collect());

                    // indexprs: expression text for expression indexes, NULL for plain column indexes
                    let indexprs = if idx.expressions.is_empty() {
                        null_val()
                    } else {
                        Value::Text(idx.expressions.join(", "))
                    };
                    let indpred = index_predicate_value(idx.predicate.as_deref());

                    let method = idx.method.as_deref();
                    let mut indclass_vals: Vec<Value> = Vec::new();
                    for col_name in &idx.columns {
                        let type_oid = schema
                            .columns
                            .iter()
                            .find(|c| &c.name == col_name)
                            .map(|c| opclass_input_type_oid(&c.data_type))
                            .unwrap_or(25); // fallback to text
                        indclass_vals.push(Value::Int64(default_opclass_oid(method, type_oid)));
                    }
                    // Expression index columns get the AM's fallback opclass
                    for _ in &idx.expressions {
                        indclass_vals.push(Value::Int64(default_opclass_oid(method, 25)));
                    }
                    let indclass = Value::Array(indclass_vals);
                    let indoption =
                        Value::Array((0..index_col_count).map(|_| Value::Int64(0)).collect());

                    rows.push(Row::new(vec![
                        int_val(index_oid),
                        int_val(base_table_oid),
                        int_val(index_col_count as i64),
                        Value::Boolean(idx.unique),
                        Value::Boolean(false),
                        Value::Boolean(false),
                        Value::Boolean(true),
                        Value::Boolean(false),
                        Value::Boolean(true),
                        Value::Boolean(false),
                        indkey,
                        indexprs,
                        indpred,
                        int_val(index_col_count as i64), // indnkeyatts
                        Value::Boolean(false),           // indnullsnotdistinct
                        indclass,
                        indoption,
                    ]));
                }

                if !schema.pk_indices.is_empty() {
                    let pk_oid = catalog_oids::pg_class_pk_index_oid(schema.table_id)?;
                    let pk_col_count = schema.pk_indices.len();

                    let indkey = schema
                        .pk_indices
                        .iter()
                        .map(|idx| (idx + 1) as i64)
                        .collect::<Vec<_>>();
                    let pk_indclass = Value::Array(
                        schema
                            .pk_indices
                            .iter()
                            .map(|&col_idx| {
                                let type_oid = schema
                                    .columns
                                    .get(col_idx)
                                    .map(|c| opclass_input_type_oid(&c.data_type))
                                    .unwrap_or(25);
                                Value::Int64(default_opclass_oid(Some("btree"), type_oid))
                            })
                            .collect(),
                    );
                    let pk_indoption =
                        Value::Array((0..pk_col_count).map(|_| Value::Int64(0)).collect());
                    rows.push(Row::new(vec![
                        int_val(pk_oid),
                        int_val(base_table_oid),
                        int_val(pk_col_count as i64),
                        Value::Boolean(true),
                        Value::Boolean(true),
                        Value::Boolean(false),
                        Value::Boolean(true),
                        Value::Boolean(false),
                        Value::Boolean(true),
                        Value::Boolean(false),
                        Value::Array(indkey.iter().map(|i| Value::Int64(*i)).collect()),
                        null_val(),                   // indexprs
                        null_val(),                   // indpred
                        int_val(pk_col_count as i64), // indnkeyatts
                        Value::Boolean(false),        // indnullsnotdistinct
                        pk_indclass,
                        pk_indoption,
                    ]));
                }
            }
        }

        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::PgIndex;
    use super::{default_opclass_oid, index_predicate_value, opclass_input_type_oid};
    use crate::model::{DataType, Value};
    use crate::sql::catalog::VirtualTable;
    use crate::sql::pg_types;

    #[test]
    fn opclass_input_type_oid_uses_anyarray_for_arrays() {
        assert_eq!(
            opclass_input_type_oid(&DataType::Array(Box::new(DataType::Int32))),
            pg_types::OID_ANYARRAY
        );
    }

    #[test]
    fn default_opclass_oid_picks_gin_array_ops_for_anyarray() {
        // 10099 is the OID for gin array_ops in pg_opclass virtual rows.
        assert_eq!(
            default_opclass_oid(Some("gin"), pg_types::OID_ANYARRAY),
            10099
        );
    }

    #[test]
    fn index_predicate_value_is_text_for_partial_index() {
        assert_eq!(
            index_predicate_value(Some("status = 'active'")),
            Value::Text("status = 'active'".to_string())
        );
    }

    #[test]
    fn index_predicate_value_is_null_for_non_partial_index() {
        assert_eq!(index_predicate_value(None), Value::Null);
    }

    #[test]
    fn pg_index_indclass_column_type_is_oidvector() {
        let schema = PgIndex.schema();
        let indclass = schema
            .columns
            .iter()
            .find(|c| c.name == "indclass")
            .expect("pg_index.indclass column must exist");
        assert_eq!(
            indclass.data_type,
            DataType::UserDefined("oidvector".to_string())
        );
    }

    #[test]
    fn pg_index_indisreplident_column_is_boolean() {
        let schema = PgIndex.schema();
        let indisreplident = schema
            .columns
            .iter()
            .find(|c| c.name == "indisreplident")
            .expect("pg_index.indisreplident column must exist");
        assert_eq!(indisreplident.data_type, DataType::Boolean);
    }
}
