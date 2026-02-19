//! ALTER/COMMENT execution helpers

use super::*;
use crate::sql::error::SqlError;

impl Executor {
    pub(crate) async fn execute_alter_owner_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let alter_owner::AlterOwnerCommand {
            kind,
            if_exists,
            name,
            new_owner,
        } = alter_owner::parse_alter_owner_sql(sql)?;

        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }

        let db_id = session.current_database_id();
        let result = async {
            let (txn, _sequence_values, search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");

            match kind {
                alter_owner::AlterOwnerKind::Table => {
                    let resolved = names::resolve_existing_table_name(
                        self.store.as_ref(),
                        txn,
                        db_id,
                        &name,
                        search_path,
                    )
                    .await?;
                    let resolved = match resolved {
                        Some(r) => r,
                        None if if_exists => {
                            return Ok(ExecuteResult::CommandComplete { tag: "ALTER TABLE" });
                        }
                        None => return Err(anyhow!("Table '{}' does not exist", name)),
                    };

                    let mut schema = self
                        .store
                        .get_schema(txn, db_id, &resolved.full)
                        .await?
                        .ok_or_else(|| anyhow!("Table '{}' does not exist", resolved.full))?;
                    schema.owner = new_owner;
                    self.store.update_schema(txn, db_id, schema).await?;
                    Ok(ExecuteResult::AlterTable {
                        table_name: resolved.full,
                    })
                }
                alter_owner::AlterOwnerKind::Sequence => {
                    let resolved = names::resolve_existing_sequence_name(
                        self.store.as_ref(),
                        txn,
                        db_id,
                        &name,
                        search_path,
                    )
                    .await?;
                    let resolved = match resolved {
                        Some(r) => r,
                        None if if_exists => {
                            return Ok(ExecuteResult::CommandComplete {
                                tag: "ALTER SEQUENCE",
                            });
                        }
                        None => return Err(anyhow!("Sequence '{}' does not exist", name)),
                    };

                    let mut seq = self
                        .store
                        .get_sequence(txn, db_id, &resolved.full)
                        .await?
                        .ok_or_else(|| anyhow!("Sequence '{}' does not exist", resolved.full))?;
                    seq.owner = new_owner;
                    self.store.update_sequence_def(txn, db_id, &seq).await?;
                    Ok(ExecuteResult::AlterSequence {
                        sequence_name: resolved.full,
                    })
                }
                alter_owner::AlterOwnerKind::Function => {
                    let resolved = names::resolve_existing_function_name(
                        self.store.as_ref(),
                        txn,
                        db_id,
                        &name,
                        search_path,
                    )
                    .await?;
                    let resolved = match resolved {
                        Some(r) => r,
                        None if if_exists => {
                            return Ok(ExecuteResult::CommandComplete {
                                tag: "ALTER FUNCTION",
                            });
                        }
                        None => return Err(anyhow!("Function '{}' does not exist", name)),
                    };

                    let mut func = self
                        .store
                        .get_function(txn, db_id, &resolved.full)
                        .await?
                        .ok_or_else(|| anyhow!("Function '{}' does not exist", resolved.full))?;
                    func.owner = new_owner;
                    self.store.replace_function(txn, db_id, func).await?;
                    Ok(ExecuteResult::AlterFunction {
                        function_name: resolved.full,
                    })
                }
            }
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

    pub(crate) async fn execute_alter_sequence_owned_by_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let alter_sequence_owned_by::AlterSequenceOwnedByCommand {
            if_exists,
            sequence_name,
            owned_by,
        } = alter_sequence_owned_by::parse_alter_sequence_owned_by_sql(sql)?;

        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }

        let db_id = session.current_database_id();
        let result = async {
            let (txn, _sequence_values, search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");

            let resolved = names::resolve_existing_sequence_name(
                self.store.as_ref(),
                txn,
                db_id,
                &sequence_name,
                search_path,
            )
            .await?;
            let resolved = match resolved {
                Some(r) => r,
                None if if_exists => {
                    return Ok(ExecuteResult::CommandComplete {
                        tag: "ALTER SEQUENCE",
                    });
                }
                None => return Err(anyhow!("Sequence '{}' does not exist", sequence_name)),
            };

            let mut seq = self
                .store
                .get_sequence(txn, db_id, &resolved.full)
                .await?
                .ok_or_else(|| anyhow!("Sequence '{}' does not exist", resolved.full))?;

            seq.owned_by = match owned_by {
                None => None,
                Some((table_name, column_name)) => {
                    let resolved_table = names::resolve_existing_table_name(
                        self.store.as_ref(),
                        txn,
                        db_id,
                        &table_name,
                        search_path,
                    )
                    .await?
                    .ok_or_else(|| anyhow!("Table '{}' does not exist", table_name))?;
                    let schema = self
                        .store
                        .get_schema(txn, db_id, &resolved_table.full)
                        .await?
                        .ok_or_else(|| anyhow!("Table '{}' does not exist", resolved_table.full))?;

                    if schema.column_index(&column_name).is_none() {
                        return Err(SqlError::ColumnNotFound {
                            column: column_name.clone(),
                            hint: None,
                        }
                        .into());
                    }

                    Some((resolved_table.full, column_name))
                }
            };

            self.store.update_sequence_def(txn, db_id, &seq).await?;
            Ok(ExecuteResult::AlterSequence {
                sequence_name: resolved.full,
            })
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

    pub(crate) async fn execute_comment_on_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let comment_on::CommentOnCommand { target, comment } =
            comment_on::parse_comment_on_sql(sql)?;

        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }

        let db_id = session.current_database_id();
        let result = async {
            let (txn, _sequence_values, search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");

            match target {
                comment_on::CommentOnTarget::Extension { name } => {
                    if self.store.get_extension(txn, db_id, &name).await?.is_none() {
                        return Err(anyhow!("extension \"{}\" does not exist", name));
                    }
                    self.store
                        .set_extension_comment(txn, db_id, &name, comment.as_deref())
                        .await?;
                }
                comment_on::CommentOnTarget::Function { name } => {
                    let resolved = names::resolve_existing_function_name(
                        self.store.as_ref(),
                        txn,
                        db_id,
                        &name,
                        search_path,
                    )
                    .await?
                    .ok_or_else(|| anyhow!("Function '{}' does not exist", name))?;

                    self.store
                        .set_function_comment(txn, db_id, &resolved.full, comment.as_deref())
                        .await?;
                }
                comment_on::CommentOnTarget::Table { name } => {
                    let resolved = names::resolve_existing_table_name(
                        self.store.as_ref(),
                        txn,
                        db_id,
                        &name,
                        search_path,
                    )
                    .await?
                    .ok_or_else(|| anyhow!("Table '{}' does not exist", name))?;

                    self.store
                        .set_table_comment(txn, db_id, &resolved.full, comment.as_deref())
                        .await?;
                }
                comment_on::CommentOnTarget::Column { table, column } => {
                    let resolved_table = names::resolve_existing_table_name(
                        self.store.as_ref(),
                        txn,
                        db_id,
                        &table,
                        search_path,
                    )
                    .await?
                    .ok_or_else(|| anyhow!("Table '{}' does not exist", table))?;
                    let schema = self
                        .store
                        .get_schema(txn, db_id, &resolved_table.full)
                        .await?
                        .ok_or_else(|| anyhow!("Table '{}' does not exist", resolved_table.full))?;

                    if schema.column_index(&column).is_none() {
                        return Err(SqlError::ColumnNotFound {
                            column: column.clone(),
                            hint: None,
                        }
                        .into());
                    }

                    self.store
                        .set_column_comment(
                            txn,
                            db_id,
                            &resolved_table.full,
                            &column,
                            comment.as_deref(),
                        )
                        .await?;
                }
            }

            Ok(ExecuteResult::CommandComplete { tag: "COMMENT" })
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
