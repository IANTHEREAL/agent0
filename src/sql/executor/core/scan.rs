//! Scan helpers

use super::*;

fn should_update_estimate(limit: Option<usize>) -> bool {
    limit.is_none()
}

fn fill_scanned_rows(rows: Vec<Row>, schema: &TableSchema) -> Result<Vec<Row>> {
    let mut filled_rows = Vec::with_capacity(rows.len());
    for mut row in rows {
        fill_row_defaults(&mut row, schema)?;
        filled_rows.push(row);
    }
    Ok(filled_rows)
}

impl Executor {
    pub(crate) async fn scan_and_fill(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_name: &str,
        schema: &TableSchema,
    ) -> Result<Vec<Row>> {
        self.scan_and_fill_with_limit(txn, db_id, table_name, schema, None)
            .await
    }

    pub(crate) async fn scan_and_fill_with_limit(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_name: &str,
        schema: &TableSchema,
        limit: Option<usize>,
    ) -> Result<Vec<Row>> {
        let rows = self.store.scan(txn, db_id, table_name, limit).await?;
        let filled_rows = fill_scanned_rows(rows, schema)?;
        if should_update_estimate(limit) {
            self.stats_cache()
                .update_estimate(db_id, schema.table_id, filled_rows.len());
        }
        Ok(filled_rows)
    }
}

#[cfg(test)]
mod tests {
    use super::{fill_scanned_rows, should_update_estimate};
    use crate::model::{ColumnDef, DataType, Row, TableSchema, Value};

    fn test_schema_with_default() -> TableSchema {
        TableSchema {
            name: "t".to_string(),
            table_id: 1,
            columns: vec![
                ColumnDef {
                    name: "id".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: true,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    generation_expr: None,
                    generation_expr_authorized_by: None,
                    collation: None,
                    is_dropped: false,
                },
                ColumnDef {
                    name: "name".to_string(),
                    data_type: DataType::Text,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: Some("'anon'".to_string()),
                    generation_expr: None,
                    generation_expr_authorized_by: None,
                    collation: None,
                    is_dropped: false,
                },
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![0],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            rls_enabled: false,
            rls_force: false,
            from_alias: None,
        }
    }

    #[test]
    fn should_update_estimate_only_when_limit_absent() {
        assert!(should_update_estimate(None));
        assert!(!should_update_estimate(Some(0)));
        assert!(!should_update_estimate(Some(10)));
    }

    #[test]
    fn fill_scanned_rows_populates_missing_defaults() {
        let schema = test_schema_with_default();
        let rows = vec![
            Row::new(vec![Value::Int32(1)]),
            Row::new(vec![Value::Int32(2)]),
        ];

        let out = fill_scanned_rows(rows, &schema).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0].values,
            vec![Value::Int32(1), Value::Text("anon".to_string())]
        );
        assert_eq!(
            out[1].values,
            vec![Value::Int32(2), Value::Text("anon".to_string())]
        );
    }
}
