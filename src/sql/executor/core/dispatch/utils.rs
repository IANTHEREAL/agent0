//! Transaction mode validation, schema drift detection, and notice collection
//! helpers for the statement dispatch layer.

use super::super::*;

/// Validate and apply transaction modes (isolation level, access mode) from
/// `BEGIN ISOLATION LEVEL ...` or `START TRANSACTION ...` statements.
///
/// Rejects SERIALIZABLE (TiKV cannot provide true serializable guarantees)
/// and stores accepted modes in the session for `SHOW` readback.
pub(in crate::sql::executor::core) fn validate_transaction_modes(
    session: &mut Session,
    modes: &[TransactionMode],
) -> Result<()> {
    for mode in modes {
        match mode {
            TransactionMode::IsolationLevel(level) => {
                let level_str = match level {
                    TransactionIsolationLevel::ReadUncommitted
                    | TransactionIsolationLevel::ReadCommitted => "read committed",
                    TransactionIsolationLevel::RepeatableRead => "repeatable read",
                    TransactionIsolationLevel::Serializable => {
                        return Err(SqlError::Unsupported(
                            "SERIALIZABLE isolation level is not supported".into(),
                        )
                        .into());
                    }
                };
                session.set_known_setting("transaction_isolation", level_str.to_string())?;
            }
            TransactionMode::AccessMode(access_mode) => {
                let mode_str = match access_mode {
                    TransactionAccessMode::ReadOnly => "on",
                    TransactionAccessMode::ReadWrite => "off",
                };
                session.set_known_setting("default_transaction_read_only", mode_str.to_string())?;
            }
        }
    }
    Ok(())
}

impl Executor {
    pub(in crate::sql::executor::core) async fn first_schema_drift_on_txn(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_versions: &[(String, u64)],
    ) -> Result<Option<(String, u64, Option<u64>)>> {
        for (table_name, expected_version) in table_versions {
            let current_schema = self.store().get_schema(txn, db_id, table_name).await?;
            let current_version = current_schema.map(|s| s.version);
            if current_version != Some(*expected_version) {
                return Ok(Some((
                    table_name.clone(),
                    *expected_version,
                    current_version,
                )));
            }
        }
        Ok(None)
    }

    pub(in crate::sql::executor::core) async fn collect_notices_before_statement(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        search_path: &[String],
        stmt: &Statement,
    ) -> Result<Vec<ExecuteResult>> {
        use sqlparser::ast::ObjectType;

        match stmt {
            Statement::Drop {
                object_type: ObjectType::Table,
                names: drop_names,
                if_exists: true,
                ..
            } => {
                let mut notices = Vec::new();
                for name in drop_names {
                    let exists = names::resolve_existing_table_name(
                        self.store.as_ref(),
                        txn,
                        db_id,
                        name,
                        search_path,
                    )
                    .await?
                    .is_some();

                    if exists {
                        continue;
                    }

                    let base = name
                        .0
                        .last()
                        .map(|ident| ident.value.as_str())
                        .unwrap_or("?");
                    notices.push(ExecuteResult::Notice {
                        message: format!("table \"{}\" does not exist, skipping", base),
                    });
                }
                Ok(notices)
            }
            _ => Ok(Vec::new()),
        }
    }
}
