//! COPY-related helpers

use super::*;

impl Executor {
    pub fn parse_value_for_copy(&self, val: &str, data_type: &DataType) -> Result<Value> {
        parse_value_for_copy(val, data_type)
    }

    pub async fn execute_copy_insert(
        &self,
        session: &mut Session,
        table_name: &str,
        col_values: Vec<(String, Value)>,
    ) -> Result<()> {
        let is_autocommit = !session.is_in_transaction();

        if is_autocommit {
            session.begin().await?;
        }

        let result = async {
            let db_id = session.current_database_id();
            let (txn, sequence_values, search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .ok_or_else(|| anyhow!("Transaction must be active"))?;
            let schema = self
                .store
                .get_schema(txn, db_id, table_name)
                .await?
                .ok_or_else(|| SqlError::RelationNotFound(table_name.to_string()))?;

            let enum_cache = dml::build_enum_label_cache(&self.store, txn, db_id, &schema).await?;

            let mut row_values = vec![Value::Null; schema.columns.len()];
            let mut indices: Vec<usize> = Vec::with_capacity(col_values.len());

            for (col_name, value) in col_values {
                if let Some(idx) = schema.column_index(&col_name) {
                    row_values[idx] = value;
                    indices.push(idx);
                }
            }
            indices.sort_unstable();
            indices.dedup();

            dml::fill_missing_columns(
                &self.store,
                txn,
                db_id,
                sequence_values,
                search_path,
                &schema,
                &mut row_values,
                &indices,
            )
            .await?;
            dml::coerce_row_values(&schema, &mut row_values)?;
            let row = Row { values: row_values };
            dml::validate_check_constraints(&schema, &row)?;

            let on_conflict = None;
            let _ = dml::execute_insert_row(
                &self.store,
                txn,
                db_id,
                table_name,
                &schema,
                row,
                &on_conflict,
                &enum_cache,
            )
            .await?;

            Ok::<(), anyhow::Error>(())
        }
        .await;

        if is_autocommit {
            if result.is_ok() {
                session.commit().await?;
            } else {
                session.rollback().await?;
            }
        }

        result
    }
}
